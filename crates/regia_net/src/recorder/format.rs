// SPDX-License-Identifier: MPL-2.0
//! `.netdemo` v1: the container of a network recording (spec 103 § 5, `docs/formats/netdemo.md`).
//!
//! The frames inside are the wire frames of one protocol, stored verbatim; the container alone has a
//! `format_version`. [`NetDemoReader`] checks everything it reads and never trusts a length: a damaged or
//! hostile file is a [`NetDemoError`], at open wherever the damage can be seen from the container.

use std::io::{Read, Seek, SeekFrom};

use regia_ecs::prelude::*;

use crate::bits::BitReader;
use crate::message::{Channel, NetMessageRegistry};
use crate::model::NetTick;
use crate::replication::ReplicationRegistry;
use crate::session::PeerId;
use crate::wire::{WireError, WireReader};

/// The first bytes of every `.netdemo`.
pub const NETDEMO_MAGIC: [u8; 8] = *b"RNETDEMO";
/// The container version this build reads and writes.
pub const NETDEMO_FORMAT_VERSION: u32 = 1;
/// The oldest container version this build reads.
pub const NETDEMO_OLDEST_FORMAT_VERSION: u32 = 1;
/// The largest chunk payload, the most one client apply may hold (`runtime::MAX_PENDING_FRAME_BYTES`).
pub const MAX_CHUNK_BYTES: usize = crate::runtime::MAX_PENDING_FRAME_BYTES;
/// The largest header body.
pub const MAX_HEADER_BYTES: usize = 1 << 20;
/// Bytes before a chunk's payload: kind, tick, length, CRC-32.
pub const CHUNK_HEADER_BYTES: usize = 13;
/// Bytes before the header body: magic, version, body length, CRC-32.
pub const FILE_PREAMBLE_BYTES: usize = 20;
/// Bytes a frame adds to a tick chunk: channel, length.
pub const FRAME_HEADER_BYTES: usize = 5;
/// The size of the end chunk.
pub const END_CHUNK_BYTES: usize = CHUNK_HEADER_BYTES + END_PAYLOAD_BYTES;

const END_PAYLOAD_BYTES: usize = 8 + 8 + 4 + 1;
const MAX_NAME_BYTES: usize = 4096;
const MAX_TABLE_ROWS: usize = 1 << 16;

/// A chunk's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkKind {
    /// One frame with the full state at its tick.
    Keyframe,
    /// The frames of one send tick.
    Tick,
    /// The last chunk: counts and the end reason.
    End,
}

impl ChunkKind {
    fn byte(self) -> u8 {
        match self {
            Self::Keyframe => 1,
            Self::Tick => 2,
            Self::End => 0xEE,
        }
    }

    fn parse(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Keyframe),
            2 => Some(Self::Tick),
            0xEE => Some(Self::End),
            _ => None,
        }
    }
}

/// Why a recording or a read failed. Each has a stable lowercase [`code`](Self::code), the agent's `data.reason`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetDemoError {
    /// The world is not a server.
    NotServer,
    /// A recording of that kind is already running.
    Busy(&'static str),
    /// No recording of that kind is running.
    Idle,
    /// The file and the ring share one connection, so they share one view.
    ViewConflict,
    /// A `Player` view named a peer that is not connected.
    ViewInvalid(u32),
    /// A value of the configuration is out of range.
    ConfigInvalid(&'static str),
    /// No ring is running.
    RingIdle,
    /// The ring holds no whole segment yet.
    RingEmpty,
    /// The target exists and `overwrite` is off.
    Exists(String),
    /// A file operation failed.
    Io(String),
    /// The bytes are not a `.netdemo`.
    NotADemo,
    /// The container version is not one this build reads.
    Version(u32),
    /// The recording belongs to another protocol.
    ProtocolMismatch(String),
    /// The file ends early.
    Truncated(String),
    /// The file is damaged.
    Corrupt(String),
    /// The world has a running network session.
    SessionActive,
    /// A replay player is already open in this world (ADR-8762): close it first.
    ReplayOpen,
    /// The target tick is outside the recording.
    SeekRange {
        /// The tick asked for.
        tick: u32,
        /// The first recorded tick.
        first: u32,
        /// The last recorded tick.
        last: u32,
    },
}

impl NetDemoError {
    /// The stable code (agent `data.reason`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotServer => "net_not_server",
            Self::Busy(_) => "recorder_busy",
            Self::Idle => "recorder_idle",
            Self::ViewConflict => "recorder_view_conflict",
            Self::ViewInvalid(_) => "recorder_view_invalid",
            Self::ConfigInvalid(_) => "recorder_config_invalid",
            Self::RingIdle => "ring_idle",
            Self::RingEmpty => "ring_empty",
            Self::Exists(_) => "netdemo_exists",
            Self::Io(_) => "netdemo_io",
            Self::NotADemo => "netdemo_not_a_demo",
            Self::Version(_) => "netdemo_version",
            Self::ProtocolMismatch(_) => "netdemo_protocol_mismatch",
            Self::Truncated(_) => "netdemo_truncated",
            Self::Corrupt(_) => "netdemo_corrupt",
            Self::SessionActive => "netdemo_session_active",
            Self::ReplayOpen => "replay_already_open",
            Self::SeekRange { .. } => "replay_seek_range",
        }
    }
}

impl std::fmt::Display for NetDemoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotServer => write!(f, "only a server records its stream"),
            Self::Busy(what) => write!(f, "a {what} recording is already running"),
            Self::Idle => write!(f, "no file recording is running"),
            Self::ViewConflict => write!(
                f,
                "the file and the ring share one connection, so they share one view"
            ),
            Self::ViewInvalid(peer) => write!(f, "peer {peer} is not connected"),
            Self::ConfigInvalid(what) => write!(f, "invalid recorder setting: {what}"),
            Self::RingIdle => write!(f, "no ring is running"),
            Self::RingEmpty => write!(f, "the ring holds no whole segment yet"),
            Self::Exists(path) => write!(f, "{path} exists"),
            Self::Io(what) => write!(f, "file error: {what}"),
            Self::NotADemo => write!(f, "not a .netdemo file"),
            Self::Version(v) => write!(
                f,
                ".netdemo format_version {v}; this build reads {NETDEMO_OLDEST_FORMAT_VERSION} to {NETDEMO_FORMAT_VERSION}"
            ),
            Self::ProtocolMismatch(what) => write!(f, "recorded with another protocol: {what}"),
            Self::Truncated(what) => write!(f, "the recording ends early: {what}"),
            Self::Corrupt(what) => write!(f, "the recording is damaged: {what}"),
            Self::SessionActive => write!(f, "this world has a running network session"),
            Self::ReplayOpen => write!(f, "a replay is already open in this world; close it first"),
            Self::SeekRange { tick, first, last } => {
                write!(f, "tick {tick} is outside the recording ({first}..={last})")
            }
        }
    }
}

impl std::error::Error for NetDemoError {}

fn io_error(e: &std::io::Error) -> NetDemoError {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        NetDemoError::Truncated(e.to_string())
    } else {
        NetDemoError::Io(e.to_string())
    }
}

fn wire_error(e: &WireError) -> NetDemoError {
    NetDemoError::Corrupt(format!("header: {e:?}"))
}

// ---- CRC ----------------------------------------------------------------------------------------

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = crc_table();

/// Feeds `bytes` to a running CRC-32 (start at `0xffff_ffff`, finish with `!`).
fn crc_update(mut crc: u32, bytes: &[u8]) -> u32 {
    for b in bytes {
        let index = usize::from((crc as u8) ^ *b);
        crc = CRC_TABLE.get(index).copied().unwrap_or(0) ^ (crc >> 8);
    }
    crc
}

/// IEEE CRC-32 of the chunk header's first 9 bytes and the payload.
fn chunk_crc(head: &[u8], payload: &[u8]) -> u32 {
    !crc_update(crc_update(0xffff_ffff, head), payload)
}

/// IEEE CRC-32 of `bytes`: the same value as `snapshot_delta::crc32_ieee`, by table.
#[must_use]
pub fn netdemo_crc32(bytes: &[u8]) -> u32 {
    !crc_update(0xffff_ffff, bytes)
}

// ---- header -------------------------------------------------------------------------------------

/// One row of the header's component table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentRow {
    /// The type's path.
    pub name: String,
    /// The codec layout hash.
    pub layout: u64,
    /// The audience variant.
    pub audience: String,
}

/// One row of the header's message table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRow {
    /// The type's path.
    pub name: String,
    /// The direction.
    pub direction: String,
    /// The channel.
    pub channel: String,
}

/// Which entities a recording holds (spec 103 § 3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecorderView {
    /// Every replicated entity.
    #[default]
    Everything,
    /// The entities this peer holds.
    Player(PeerId),
}

impl RecorderView {
    /// `everything` or `player:<id>`.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Everything => "everything".to_string(),
            Self::Player(p) => format!("player:{}", p.0),
        }
    }
}

/// A `.netdemo` header.
#[derive(Debug, Clone, PartialEq)]
pub struct NetDemoHeader {
    /// `NET_PROTOCOL_VERSION` of the recording build.
    pub net_protocol: u32,
    /// `runtime::protocol_hash` of the recording world.
    pub protocol_hash: u64,
    /// Server ticks per second.
    pub tick_hz: f64,
    /// Ticks between send ticks.
    pub send_every_ticks: u32,
    /// Ticks between keyframes.
    pub keyframe_every_ticks: u32,
    /// The first recorded tick.
    pub first_tick: NetTick,
    /// Which entities it holds.
    pub view: RecorderView,
    /// The level it was recorded on (one file holds one level).
    pub level: String,
    /// The engine version that wrote it.
    pub engine_version: String,
    /// The registered components, in order.
    pub components: Vec<ComponentRow>,
    /// The registered messages, in order.
    pub messages: Vec<MessageRow>,
}

fn channel_name(c: Channel) -> &'static str {
    match c {
        Channel::ReliableOrdered => "reliable_ordered",
        Channel::ReliableUnordered => "reliable_unordered",
        Channel::Unreliable => "unreliable",
        Channel::UnreliableSequenced => "unreliable_sequenced",
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    // On a character's edge: the reader refuses text that is not UTF-8.
    let s = s.get(..s.floor_char_boundary(MAX_NAME_BYTES)).unwrap_or_default();
    out.extend_from_slice(&(s.len() as u16).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn get_str(r: &mut WireReader) -> Result<String, NetDemoError> {
    let n = usize::from(r.u16().map_err(|e| wire_error(&e))?);
    if n > MAX_NAME_BYTES {
        return Err(NetDemoError::Corrupt("header: a name is too long".to_string()));
    }
    let bytes = r.take(n).map_err(|e| wire_error(&e))?;
    std::str::from_utf8(bytes)
        .map(str::to_string)
        .map_err(|_| NetDemoError::Corrupt("header: a name is not UTF-8".to_string()))
}

impl NetDemoHeader {
    /// The tables of this world, in registry order: the part of a header a playing world must match.
    #[must_use]
    pub fn tables_of(world: &World) -> (Vec<ComponentRow>, Vec<MessageRow>) {
        let components = world
            .get_resource::<ReplicationRegistry>()
            .map(|r| {
                r.components()
                    .iter()
                    .map(|c| ComponentRow {
                        name: c.name.to_string(),
                        layout: c.layout,
                        audience: c.options.audience.as_str().to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let messages = world
            .get_resource::<NetMessageRegistry>()
            .map(|r| {
                r.types()
                    .iter()
                    .map(|t| MessageRow {
                        name: t.name.to_string(),
                        direction: t.direction.as_str().to_string(),
                        channel: channel_name(t.channel).to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        (components, messages)
    }

    /// Refuses a world that registered another protocol, naming the first row that differs.
    ///
    /// # Errors
    /// [`NetDemoError::ProtocolMismatch`].
    pub fn check_world(&self, world: &World) -> Result<(), NetDemoError> {
        if self.net_protocol != crate::settings::NET_PROTOCOL_VERSION {
            return Err(NetDemoError::ProtocolMismatch(format!(
                "protocol version {}, this build speaks {}",
                self.net_protocol,
                crate::settings::NET_PROTOCOL_VERSION
            )));
        }
        let hash = crate::runtime::protocol_hash(world);
        if self.protocol_hash == hash {
            return Ok(());
        }
        let (components, messages) = Self::tables_of(world);
        let mut detail = format!(
            "protocol hash {:016x}, this world has {hash:016x}",
            self.protocol_hash
        );
        for i in 0..self.components.len().max(components.len()) {
            let (a, b) = (self.components.get(i), components.get(i));
            if a != b {
                detail = format!(
                    "component {i}: recorded {}, this world has {}",
                    a.map_or("nothing", |r| r.name.as_str()),
                    b.map_or("nothing", |r| r.name.as_str())
                );
                return Err(NetDemoError::ProtocolMismatch(detail));
            }
        }
        for i in 0..self.messages.len().max(messages.len()) {
            let (a, b) = (self.messages.get(i), messages.get(i));
            if a != b {
                detail = format!(
                    "message {i}: recorded {}, this world has {}",
                    a.map_or("nothing", |r| r.name.as_str()),
                    b.map_or("nothing", |r| r.name.as_str())
                );
                return Err(NetDemoError::ProtocolMismatch(detail));
            }
        }
        Err(NetDemoError::ProtocolMismatch(detail))
    }

    /// The whole file preamble and header body.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(512);
        body.extend_from_slice(&self.net_protocol.to_le_bytes());
        body.extend_from_slice(&self.protocol_hash.to_le_bytes());
        body.extend_from_slice(&self.tick_hz.to_bits().to_le_bytes());
        body.extend_from_slice(&self.send_every_ticks.to_le_bytes());
        body.extend_from_slice(&self.keyframe_every_ticks.to_le_bytes());
        body.extend_from_slice(&self.first_tick.0.to_le_bytes());
        match self.view {
            RecorderView::Everything => {
                body.push(0);
                body.extend_from_slice(&0u32.to_le_bytes());
            }
            RecorderView::Player(p) => {
                body.push(1);
                body.extend_from_slice(&p.0.to_le_bytes());
            }
        }
        put_str(&mut body, &self.level);
        put_str(&mut body, &self.engine_version);
        body.extend_from_slice(&(self.components.len() as u32).to_le_bytes());
        for c in &self.components {
            put_str(&mut body, &c.name);
            body.extend_from_slice(&c.layout.to_le_bytes());
            put_str(&mut body, &c.audience);
        }
        body.extend_from_slice(&(self.messages.len() as u32).to_le_bytes());
        for m in &self.messages {
            put_str(&mut body, &m.name);
            put_str(&mut body, &m.direction);
            put_str(&mut body, &m.channel);
        }
        let mut out = Vec::with_capacity(FILE_PREAMBLE_BYTES + body.len());
        out.extend_from_slice(&NETDEMO_MAGIC);
        out.extend_from_slice(&NETDEMO_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&netdemo_crc32(&body).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    fn decode_body(body: &[u8]) -> Result<Self, NetDemoError> {
        let mut r = WireReader::new(body);
        let w = |e: WireError| wire_error(&e);
        let net_protocol = r.u32().map_err(w)?;
        let protocol_hash = r.u64().map_err(w)?;
        let tick_hz = f64::from_bits(r.u64().map_err(w)?);
        if !tick_hz.is_finite() || tick_hz <= 0.0 || tick_hz > 100_000.0 {
            return Err(NetDemoError::Corrupt("header: tick rate".to_string()));
        }
        let send_every_ticks = r.u32().map_err(w)?;
        let keyframe_every_ticks = r.u32().map_err(w)?;
        if send_every_ticks == 0 || keyframe_every_ticks == 0 {
            return Err(NetDemoError::Corrupt("header: cadence".to_string()));
        }
        let first_tick = NetTick(r.u32().map_err(w)?);
        let kind = r.u8().map_err(w)?;
        let peer = r.u32().map_err(w)?;
        let view = match kind {
            0 => RecorderView::Everything,
            1 => RecorderView::Player(PeerId(peer)),
            _ => return Err(NetDemoError::Corrupt("header: view".to_string())),
        };
        let level = get_str(&mut r)?;
        let engine_version = get_str(&mut r)?;
        let rows = |n: u32| -> Result<usize, NetDemoError> {
            let n = n as usize;
            if n > MAX_TABLE_ROWS {
                return Err(NetDemoError::Corrupt("header: table size".to_string()));
            }
            Ok(n)
        };
        let n = rows(r.u32().map_err(w)?)?;
        let mut components = Vec::with_capacity(n.min(r.remaining() / 12));
        for _ in 0..n {
            let name = get_str(&mut r)?;
            let layout = r.u64().map_err(w)?;
            let audience = get_str(&mut r)?;
            components.push(ComponentRow {
                name,
                layout,
                audience,
            });
        }
        let n = rows(r.u32().map_err(w)?)?;
        let mut messages = Vec::with_capacity(n.min(r.remaining() / 6));
        for _ in 0..n {
            let name = get_str(&mut r)?;
            let direction = get_str(&mut r)?;
            let channel = get_str(&mut r)?;
            messages.push(MessageRow {
                name,
                direction,
                channel,
            });
        }
        if !r.is_empty() {
            return Err(NetDemoError::Corrupt("header: trailing bytes".to_string()));
        }
        Ok(Self {
            net_protocol,
            protocol_hash,
            tick_hz,
            send_every_ticks,
            keyframe_every_ticks,
            first_tick,
            view,
            level,
            engine_version,
            components,
            messages,
        })
    }
}

// ---- chunks -------------------------------------------------------------------------------------

/// Why a recording ended (the end chunk says it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EndReason {
    /// `stop_recording`, or a ring export.
    #[default]
    Stopped,
    /// `max_bytes`.
    SizeCap,
    /// `max_seconds`.
    TimeCap,
    /// The session traveled to another level.
    LevelChanged,
    /// The disk could not keep up.
    DiskSlow,
    /// The tick counter ran out.
    TicksExhausted,
}

impl EndReason {
    fn byte(self) -> u8 {
        match self {
            Self::Stopped => 0,
            Self::SizeCap => 1,
            Self::TimeCap => 2,
            Self::LevelChanged => 3,
            Self::DiskSlow => 4,
            Self::TicksExhausted => 5,
        }
    }

    fn parse(byte: u8) -> Option<Self> {
        Some(match byte {
            0 => Self::Stopped,
            1 => Self::SizeCap,
            2 => Self::TimeCap,
            3 => Self::LevelChanged,
            4 => Self::DiskSlow,
            5 => Self::TicksExhausted,
            _ => return None,
        })
    }

    /// The lowercase name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::SizeCap => "size_cap",
            Self::TimeCap => "time_cap",
            Self::LevelChanged => "level_changed",
            Self::DiskSlow => "disk_slow",
            Self::TicksExhausted => "ticks_exhausted",
        }
    }
}

/// Starts a chunk in `out`: its header with the length and CRC still zero.
pub(crate) fn begin_chunk(out: &mut Vec<u8>, kind: ChunkKind, tick: NetTick) -> usize {
    let start = out.len();
    out.push(kind.byte());
    out.extend_from_slice(&tick.0.to_le_bytes());
    out.extend_from_slice(&[0; 8]);
    start
}

/// Finishes the chunk begun at `start`: fills in the length and the CRC. False when it is over the limit, and then
/// the chunk is gone from `out`.
pub(crate) fn finish_chunk(out: &mut Vec<u8>, start: usize) -> bool {
    let payload_at = start + CHUNK_HEADER_BYTES;
    let len = out.len().saturating_sub(payload_at);
    if len > MAX_CHUNK_BYTES {
        out.truncate(start);
        return false;
    }
    if let Some(field) = out.get_mut(start + 5..start + 9) {
        field.copy_from_slice(&(len as u32).to_le_bytes());
    }
    let crc = {
        let (head, payload) = match (out.get(start..start + 9), out.get(payload_at..)) {
            (Some(h), Some(p)) => (h, p),
            _ => return false,
        };
        chunk_crc(head, payload)
    };
    if let Some(field) = out.get_mut(start + 9..start + 13) {
        field.copy_from_slice(&crc.to_le_bytes());
    }
    true
}

/// Appends a frame to a tick chunk's payload.
pub(crate) fn put_frame(out: &mut Vec<u8>, channel: Channel, bytes: &[u8]) {
    out.push(channel_byte(channel));
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn channel_byte(c: Channel) -> u8 {
    match c {
        Channel::ReliableOrdered => 0,
        Channel::ReliableUnordered => 1,
        Channel::Unreliable => 2,
        Channel::UnreliableSequenced => 3,
    }
}

fn channel_from(b: u8) -> Option<Channel> {
    Some(match b {
        0 => Channel::ReliableOrdered,
        1 => Channel::ReliableUnordered,
        2 => Channel::Unreliable,
        3 => Channel::UnreliableSequenced,
        _ => return None,
    })
}

/// Writes the end chunk into `out`.
pub(crate) fn put_end_chunk(out: &mut Vec<u8>, summary: &EndSummary) {
    let start = begin_chunk(out, ChunkKind::End, summary.last_tick);
    out.extend_from_slice(&summary.tick_chunks.to_le_bytes());
    out.extend_from_slice(&summary.keyframes.to_le_bytes());
    out.extend_from_slice(&summary.last_tick.0.to_le_bytes());
    out.push(summary.reason.byte());
    let _ = finish_chunk(out, start);
}

/// What the end chunk says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EndSummary {
    /// Tick chunks in the file.
    pub tick_chunks: u64,
    /// Keyframes in the file.
    pub keyframes: u64,
    /// The last recorded tick.
    pub last_tick: NetTick,
    /// Why it ended.
    pub reason: EndReason,
}

/// The tick a frame says it carries (`TICK` and `UMSG` start the same way).
pub(crate) fn frame_tick(bytes: &[u8]) -> Result<NetTick, NetDemoError> {
    let mut r = BitReader::new(bytes);
    let corrupt = |_| NetDemoError::Corrupt("a frame has no header".to_string());
    r.read_bits(8).map_err(corrupt)?;
    let tick = r.read_varint().map_err(corrupt)?;
    u32::try_from(tick)
        .map(NetTick)
        .map_err(|_| NetDemoError::Corrupt("a frame's tick is out of range".to_string()))
}

/// The frames of a tick chunk's payload.
pub fn frames_of(payload: &[u8]) -> FrameIter<'_> {
    FrameIter { rest: payload }
}

/// Walks a tick chunk's payload; see [`frames_of`].
#[derive(Debug)]
pub struct FrameIter<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for FrameIter<'a> {
    type Item = Result<(Channel, &'a [u8]), NetDemoError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let bad = |what: &str| Some(Err(NetDemoError::Corrupt(format!("tick chunk: {what}"))));
        let Some((head, body)) = self.rest.split_at_checked(FRAME_HEADER_BYTES) else {
            self.rest = &[];
            return bad("a frame header is cut");
        };
        let mut r = WireReader::new(head);
        let (channel, len) = match (r.u8(), r.u32()) {
            (Ok(c), Ok(l)) => (c, l as usize),
            _ => {
                self.rest = &[];
                return bad("a frame header is cut");
            }
        };
        let Some(channel) = channel_from(channel) else {
            self.rest = &[];
            return bad("a frame's channel is unknown");
        };
        let Some((frame, rest)) = body.split_at_checked(len) else {
            self.rest = &[];
            return bad("a frame is longer than its chunk");
        };
        self.rest = rest;
        Some(Ok((channel, frame)))
    }
}

// ---- reader -------------------------------------------------------------------------------------

/// A keyframe's place in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyframeEntry {
    /// Its tick.
    pub tick: NetTick,
    /// The byte offset of its chunk.
    pub offset: u64,
}

/// A chunk's header as read from the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkHead {
    /// Its kind.
    pub kind: ChunkKind,
    /// Its tick.
    pub tick: NetTick,
    /// Its payload length.
    pub len: u32,
    /// The payload's checksum input.
    crc: u32,
    /// The offset of the next chunk.
    pub next: u64,
}

/// An opened, checked `.netdemo`. Holds the keyframe index and no chunk data.
#[derive(Debug)]
pub struct NetDemoReader<R> {
    source: R,
    header: NetDemoHeader,
    keyframes: Vec<KeyframeEntry>,
    end: EndSummary,
    first_chunk: u64,
    file_len: u64,
    tick_chunks: u64,
    scratch: Vec<u8>,
}

impl<R: Read + Seek> NetDemoReader<R> {
    /// Reads the header and walks every chunk: lengths, checksums, order, counts.
    ///
    /// # Errors
    /// [`NetDemoError::NotADemo`], `Version`, `Truncated`, `Corrupt` or `Io`.
    pub fn open(mut source: R) -> Result<Self, NetDemoError> {
        let file_len = source.seek(SeekFrom::End(0)).map_err(|e| io_error(&e))?;
        source.seek(SeekFrom::Start(0)).map_err(|e| io_error(&e))?;
        if file_len < NETDEMO_MAGIC.len() as u64 {
            return Err(NetDemoError::NotADemo);
        }
        let mut magic = [0u8; 8];
        source.read_exact(&mut magic).map_err(|e| io_error(&e))?;
        if magic != NETDEMO_MAGIC {
            return Err(NetDemoError::NotADemo);
        }
        let mut preamble = [0u8; 12];
        source
            .read_exact(&mut preamble)
            .map_err(|_| NetDemoError::Truncated("the header is cut".to_string()))?;
        let mut r = WireReader::new(&preamble);
        let w = |e: WireError| wire_error(&e);
        let version = r.u32().map_err(w)?;
        let body_len = r.u32().map_err(w)? as usize;
        let body_crc = r.u32().map_err(w)?;
        if !(NETDEMO_OLDEST_FORMAT_VERSION..=NETDEMO_FORMAT_VERSION).contains(&version) {
            return Err(NetDemoError::Version(version));
        }
        if body_len > MAX_HEADER_BYTES {
            return Err(NetDemoError::Corrupt("the header is too long".to_string()));
        }
        if (FILE_PREAMBLE_BYTES + body_len) as u64 > file_len {
            return Err(NetDemoError::Truncated("the header is cut".to_string()));
        }
        let mut body = vec![0u8; body_len];
        source
            .read_exact(&mut body)
            .map_err(|_| NetDemoError::Truncated("the header is cut".to_string()))?;
        if netdemo_crc32(&body) != body_crc {
            return Err(NetDemoError::Corrupt(
                "the header checksum does not match".to_string(),
            ));
        }
        let header = NetDemoHeader::decode_body(&body)?;
        let first_chunk = (FILE_PREAMBLE_BYTES + body_len) as u64;
        let mut reader = Self {
            source,
            header,
            keyframes: Vec::new(),
            end: EndSummary::default(),
            first_chunk,
            file_len,
            tick_chunks: 0,
            scratch: Vec::new(),
        };
        reader.walk()?;
        Ok(reader)
    }

    fn walk(&mut self) -> Result<(), NetDemoError> {
        let mut at = self.first_chunk;
        let mut last_tick: Option<NetTick> = None;
        let mut last_key: Option<NetTick> = None;
        let mut buf = std::mem::take(&mut self.scratch);
        let result = loop {
            if at >= self.file_len {
                break Err(NetDemoError::Truncated("the end chunk is missing".to_string()));
            }
            let head = match self.read_chunk(at, &mut buf) {
                Ok(h) => h,
                Err(e) => break Err(e),
            };
            match head.kind {
                ChunkKind::Keyframe => {
                    if last_key.is_some_and(|k| head.tick <= k) || last_tick.is_some_and(|t| head.tick <= t) {
                        break Err(NetDemoError::Corrupt("keyframes out of order".to_string()));
                    }
                    if let Err(e) = check_frame_tick(&buf, head.tick) {
                        break Err(e);
                    }
                    self.keyframes.push(KeyframeEntry {
                        tick: head.tick,
                        offset: at,
                    });
                    last_key = Some(head.tick);
                }
                ChunkKind::Tick => {
                    if self.keyframes.is_empty() {
                        break Err(NetDemoError::Corrupt(
                            "the first chunk is not a keyframe".to_string(),
                        ));
                    }
                    if last_tick.is_some_and(|t| head.tick <= t) || last_key.is_some_and(|k| head.tick < k) {
                        break Err(NetDemoError::Corrupt("ticks out of order".to_string()));
                    }
                    let mut frames_ok = Ok(());
                    for frame in frames_of(&buf) {
                        match frame.and_then(|(_, f)| check_frame_tick(f, head.tick)) {
                            Ok(()) => {}
                            Err(e) => {
                                frames_ok = Err(e);
                                break;
                            }
                        }
                    }
                    if let Err(e) = frames_ok {
                        break Err(e);
                    }
                    self.tick_chunks += 1;
                    last_tick = Some(head.tick);
                }
                ChunkKind::End => {
                    let end = match parse_end(&buf) {
                        Ok(e) => e,
                        Err(e) => break Err(e),
                    };
                    if head.next != self.file_len {
                        break Err(NetDemoError::Corrupt("bytes after the end chunk".to_string()));
                    }
                    if end.tick_chunks != self.tick_chunks
                        || end.keyframes != self.keyframes.len() as u64
                        || Some(end.last_tick) != last_tick
                        || self.keyframes.is_empty()
                    {
                        break Err(NetDemoError::Corrupt(
                            "the end chunk's counts do not match the file".to_string(),
                        ));
                    }
                    if self.keyframes.first().map(|k| k.tick) != Some(self.header.first_tick) {
                        break Err(NetDemoError::Corrupt(
                            "the first keyframe is not at the first tick".to_string(),
                        ));
                    }
                    self.end = end;
                    break Ok(());
                }
            }
            at = head.next;
        };
        buf.clear();
        self.scratch = buf;
        result
    }

    /// Reads and checks the chunk at `offset`, its payload into `payload`.
    ///
    /// # Errors
    /// `Truncated` when it does not fit the file, `Corrupt` when a field or the checksum is wrong.
    pub fn read_chunk(&mut self, offset: u64, payload: &mut Vec<u8>) -> Result<ChunkHead, NetDemoError> {
        let head = self.read_head(offset)?;
        // A length was checked against the bytes left, so this cannot allocate past the file.
        payload.clear();
        payload.resize(head.len as usize, 0);
        self.source.read_exact(payload).map_err(|e| io_error(&e))?;
        let mut crc_head = [0u8; 9];
        crc_head[0] = head.kind.byte();
        if let Some(field) = crc_head.get_mut(1..5) {
            field.copy_from_slice(&head.tick.0.to_le_bytes());
        }
        if let Some(field) = crc_head.get_mut(5..9) {
            field.copy_from_slice(&head.len.to_le_bytes());
        }
        if chunk_crc(&crc_head, payload) != head.crc {
            return Err(NetDemoError::Corrupt(format!(
                "the checksum of the chunk at byte {offset} does not match"
            )));
        }
        Ok(head)
    }

    /// Reads a chunk's header only, leaving the source at its payload.
    ///
    /// # Errors
    /// `Truncated` or `Corrupt`.
    pub fn read_head(&mut self, offset: u64) -> Result<ChunkHead, NetDemoError> {
        if offset.saturating_add(CHUNK_HEADER_BYTES as u64) > self.file_len {
            return Err(NetDemoError::Truncated(format!(
                "a chunk header at byte {offset} is cut"
            )));
        }
        self.source
            .seek(SeekFrom::Start(offset))
            .map_err(|e| io_error(&e))?;
        let mut h = [0u8; CHUNK_HEADER_BYTES];
        self.source.read_exact(&mut h).map_err(|e| io_error(&e))?;
        let mut r = WireReader::new(&h);
        let w = |e: WireError| wire_error(&e);
        let kind = r.u8().map_err(w)?;
        let tick = r.u32().map_err(w)?;
        let len = r.u32().map_err(w)?;
        let crc = r.u32().map_err(w)?;
        let Some(kind) = ChunkKind::parse(kind) else {
            return Err(NetDemoError::Corrupt(format!(
                "unknown chunk kind {kind} at byte {offset}"
            )));
        };
        if len as usize > MAX_CHUNK_BYTES {
            return Err(NetDemoError::Corrupt(format!(
                "a chunk of {len} bytes at byte {offset}"
            )));
        }
        let next = offset + CHUNK_HEADER_BYTES as u64 + u64::from(len);
        if next > self.file_len {
            return Err(NetDemoError::Truncated(format!(
                "the chunk at byte {offset} is cut"
            )));
        }
        Ok(ChunkHead {
            kind,
            tick: NetTick(tick),
            len,
            crc,
            next,
        })
    }

    /// The header.
    #[must_use]
    pub fn header(&self) -> &NetDemoHeader {
        &self.header
    }

    /// The keyframe index, in tick order.
    #[must_use]
    pub fn keyframes(&self) -> &[KeyframeEntry] {
        &self.keyframes
    }

    /// The newest keyframe at or before `tick`: always one, because the file starts with one.
    #[must_use]
    pub fn keyframe_at_or_before(&self, tick: NetTick) -> Option<KeyframeEntry> {
        let n = self.keyframes.partition_point(|k| k.tick <= tick);
        self.keyframes
            .get(n.saturating_sub(1))
            .copied()
            .filter(|k| k.tick <= tick)
    }

    /// The end chunk.
    #[must_use]
    pub fn end(&self) -> EndSummary {
        self.end
    }

    /// The offset of the first chunk.
    #[must_use]
    pub fn first_chunk(&self) -> u64 {
        self.first_chunk
    }

    /// The first recorded tick.
    #[must_use]
    pub fn first_tick(&self) -> NetTick {
        self.header.first_tick
    }

    /// The last recorded tick.
    #[must_use]
    pub fn last_tick(&self) -> NetTick {
        self.end.last_tick
    }

    /// The file's size.
    #[must_use]
    pub fn file_len(&self) -> u64 {
        self.file_len
    }
}

fn check_frame_tick(frame: &[u8], tick: NetTick) -> Result<(), NetDemoError> {
    if frame_tick(frame)? != tick {
        return Err(NetDemoError::Corrupt(
            "a frame's tick is not its chunk's tick".to_string(),
        ));
    }
    Ok(())
}

fn parse_end(payload: &[u8]) -> Result<EndSummary, NetDemoError> {
    if payload.len() != END_PAYLOAD_BYTES {
        return Err(NetDemoError::Corrupt("the end chunk's size".to_string()));
    }
    let mut r = WireReader::new(payload);
    let w = |e: WireError| wire_error(&e);
    let tick_chunks = r.u64().map_err(w)?;
    let keyframes = r.u64().map_err(w)?;
    let last_tick = NetTick(r.u32().map_err(w)?);
    let Some(reason) = EndReason::parse(r.u8().map_err(w)?) else {
        return Err(NetDemoError::Corrupt("the end chunk's reason".to_string()));
    };
    Ok(EndSummary {
        tick_chunks,
        keyframes,
        last_tick,
        reason,
    })
}

// ---- schema fingerprint -------------------------------------------------------------------------

/// The layout fingerprint for `cargo xtask check-formats`: the binary layout has no serde type, so this names its
/// fields. A change here is a format version bump.
#[must_use]
pub fn netdemo_schema_fingerprint() -> String {
    concat!(
        "preamble:magic,format_version,body_len,body_crc;",
        "body:net_protocol,protocol_hash,tick_hz,send_every_ticks,keyframe_every_ticks,first_tick,view_kind,view_peer,",
        "level,engine_version,components[name,layout,audience],messages[name,direction,channel];",
        "chunk:kind,tick,len,crc,payload;kinds:1=keyframe,2=tick[channel,len,frame],238=end[tick_chunks,keyframes,last_tick,reason]",
    )
    .to_string()
}

/// The `from` versions of the `.netdemo` migrations: none, the format has one version.
#[must_use]
pub fn netdemo_migration_sources() -> Vec<u32> {
    (NETDEMO_OLDEST_FORMAT_VERSION..NETDEMO_FORMAT_VERSION).collect()
}

/// The `format_version` of a `.netdemo`'s bytes, for `regia upgrade-project`: `Ok(None)` when they are not one.
///
/// # Errors
/// A reason when the magic is there and the version is cut.
pub fn netdemo_file_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    if bytes.len() < NETDEMO_MAGIC.len() || bytes.get(..NETDEMO_MAGIC.len()) != Some(&NETDEMO_MAGIC[..]) {
        return Ok(None);
    }
    let Some(version) = bytes.get(NETDEMO_MAGIC.len()..NETDEMO_MAGIC.len() + 4) else {
        return Err("the header is cut".to_string());
    };
    let mut r = WireReader::new(version);
    r.u32().map(Some).map_err(|e| format!("{e:?}"))
}

/// Reads a whole file the way a player does: the container, each chunk, every frame through the client path's walk. The
/// result is the file again, with its header written from what was read: what the fuzz target's round trip holds to.
///
/// # Errors
/// As [`NetDemoReader::open`] and [`NetDemoReader::read_chunk`].
pub fn read_whole(bytes: &[u8]) -> Result<Vec<u8>, NetDemoError> {
    let mut reader = NetDemoReader::open(std::io::Cursor::new(bytes))?;
    let mut at = reader.first_chunk();
    let mut payload = Vec::new();
    while at < reader.file_len() {
        let head = reader.read_chunk(at, &mut payload)?;
        match head.kind {
            ChunkKind::Tick => {
                for frame in frames_of(&payload) {
                    let (_, frame) = frame?;
                    super::playback::validate_frame(frame)?;
                }
            }
            ChunkKind::Keyframe => super::playback::validate_frame(&payload)?,
            ChunkKind::End => {}
        }
        at = head.next;
    }
    let mut out = reader.header().encode();
    out.extend_from_slice(bytes.get(reader.first_chunk() as usize..).unwrap_or_default());
    Ok(out)
}

/// The file with its checksums made right, wherever its lengths let them be found, so a mutation reaches what the
/// checksums otherwise stop at the door: the header's tables and the frames. Malformed input stays malformed.
#[must_use]
pub fn resealed(bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    let word = |b: &[u8], at: usize| -> Option<usize> {
        let w: [u8; 4] = b.get(at..at + 4)?.try_into().ok()?;
        Some(u32::from_le_bytes(w) as usize)
    };
    let Some(body) = word(&out, 12) else {
        return out;
    };
    let Some(after_body) = (FILE_PREAMBLE_BYTES)
        .checked_add(body)
        .filter(|e| *e <= out.len())
    else {
        return out;
    };
    let crc = netdemo_crc32(out.get(FILE_PREAMBLE_BYTES..after_body).unwrap_or_default());
    if let Some(field) = out.get_mut(16..20) {
        field.copy_from_slice(&crc.to_le_bytes());
    }
    let mut at = after_body;
    while let Some(len) = word(&out, at + 5) {
        let start = at + CHUNK_HEADER_BYTES;
        let Some(end) = start.checked_add(len).filter(|e| *e <= out.len()) else {
            break;
        };
        let mut signed = out.get(at..at + 9).unwrap_or_default().to_vec();
        signed.extend_from_slice(out.get(start..end).unwrap_or_default());
        let crc = netdemo_crc32(&signed);
        if let Some(field) = out.get_mut(at + 9..at + 13) {
            field.copy_from_slice(&crc.to_le_bytes());
        }
        at = end;
    }
    out
}

/// A small valid file: a keyframe and two ticks, one with a message list. The fuzz target's fixture and a reference
/// for `docs/formats/netdemo.md`; a real recording is built the same way.
#[must_use]
pub fn fixture_netdemo() -> Vec<u8> {
    let frame = |tick: u32, spawn: bool| {
        let mut w = crate::bits::BitWriter::new();
        w.write_bits(u64::from(crate::runtime::TAG_TICK), 8);
        w.write_varint(u64::from(tick));
        // No ownership changes.
        w.write_varint(0);
        if spawn {
            // One bare spawn, id 1, no parent, not owned, no transform, no components.
            w.write_varint(1);
            w.write_varint(1);
            w.write_bits(0, 2);
            w.write_bool(false);
            w.write_bool(false);
            w.write_bool(false);
            w.write_bits(0, 2);
            w.write_varint(0);
        } else {
            w.write_varint(0);
        }
        // No updates, despawns, removed level entities or messages.
        for _ in 0..4 {
            w.write_varint(0);
        }
        w.finish()
    };
    let header = NetDemoHeader {
        net_protocol: crate::settings::NET_PROTOCOL_VERSION,
        protocol_hash: 0x1234_5678_9abc_def0,
        tick_hz: 60.0,
        send_every_ticks: 1,
        keyframe_every_ticks: 300,
        first_tick: NetTick(10),
        view: RecorderView::Everything,
        level: "fixture".to_string(),
        engine_version: "fixture".to_string(),
        components: vec![ComponentRow {
            name: "fixture::Health".to_string(),
            layout: 7,
            audience: "all".to_string(),
        }],
        messages: vec![MessageRow {
            name: "fixture::Boom".to_string(),
            direction: "broadcast".to_string(),
            channel: "reliable_ordered".to_string(),
        }],
    };
    let mut out = header.encode();
    let start = begin_chunk(&mut out, ChunkKind::Keyframe, NetTick(10));
    out.extend_from_slice(&frame(10, true));
    finish_chunk(&mut out, start);
    for (tick, spawn) in [(10, true), (11, false)] {
        let start = begin_chunk(&mut out, ChunkKind::Tick, NetTick(tick));
        put_frame(&mut out, Channel::ReliableOrdered, &frame(tick, spawn));
        finish_chunk(&mut out, start);
    }
    put_end_chunk(
        &mut out,
        &EndSummary {
            tick_chunks: 2,
            keyframes: 1,
            last_tick: NetTick(11),
            reason: EndReason::Stopped,
        },
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_crc_is_the_packet_crc() {
        for bytes in [&b""[..], b"a", b"123456789", &[0xff; 1000]] {
            assert_eq!(netdemo_crc32(bytes), crate::snapshot_delta::crc32_ieee(bytes));
        }
        assert_eq!(netdemo_crc32(b"123456789"), 0xcbf4_3926, "the check value");
    }

    #[test]
    fn the_fixture_reads_and_round_trips() {
        let file = fixture_netdemo();
        assert_eq!(read_whole(&file).expect("reads"), file);
        assert_eq!(netdemo_file_version(&file), Ok(Some(NETDEMO_FORMAT_VERSION)));
        assert_eq!(netdemo_file_version(b"RFRC...."), Ok(None));
        let reader = NetDemoReader::open(std::io::Cursor::new(file)).expect("opens");
        assert_eq!(reader.first_tick(), NetTick(10));
        assert_eq!(reader.last_tick(), NetTick(11));
        assert_eq!(reader.keyframes().len(), 1);
        assert_eq!(
            reader.keyframe_at_or_before(NetTick(11)).map(|k| k.tick),
            Some(NetTick(10))
        );
        assert_eq!(reader.keyframe_at_or_before(NetTick(9)), None);
        assert_eq!(reader.end().reason, EndReason::Stopped);
    }

    #[test]
    fn the_header_round_trips_with_a_player_view() {
        let file = fixture_netdemo();
        let mut reader = NetDemoReader::open(std::io::Cursor::new(file)).expect("opens");
        let mut header = reader.header().clone();
        let mut payload = Vec::new();
        reader
            .read_chunk(reader.first_chunk(), &mut payload)
            .expect("reads");
        header.view = RecorderView::Player(PeerId(4));
        let encoded = header.encode();
        let body = encoded.get(FILE_PREAMBLE_BYTES..).expect("a body");
        assert_eq!(NetDemoHeader::decode_body(body), Ok(header));
    }
}
