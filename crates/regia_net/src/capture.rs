// SPDX-License-Identifier: MPL-2.0
//! Client capture with timing (spec 103 § 14, spec 120 § 15.5, ADR-8221).
//!
//! The recorder of spec 103 is a virtual connection on the server: it has no socket and no timing,
//! so it cannot say why one client lagged. A client capture is the other half. A [`NetClient`]
//! with a [`CaptureTap`] logs, in the client's own milliseconds:
//!
//! | Record | When |
//! |---|---|
//! | [`CaptureRecord::Session`] | the tap is attached to a new client (one connection attempt) |
//! | [`CaptureRecord::Receive`] | each `NetClient::receive` (the read of the socket, the frame's time) |
//! | [`CaptureRecord::Accept`], [`CaptureRecord::Deny`], [`CaptureRecord::Goodbye`] | the server's answer, after decryption |
//! | [`CaptureRecord::Packet`] | each authenticated connection packet, after decryption and the token check |
//! | [`CaptureRecord::Input`] | each `NetClient::send_input`: the stamp and the encoded input |
//! | [`CaptureRecord::Flush`] | each `NetClient::flush` |
//! | [`CaptureRecord::Ended`] | the client stopped, with its end code |
//! | [`CaptureRecord::Frame`] | the game's frame marker, with its time and a digest of its output |
//!
//! Records go into a [`ClientCapture`]: a ring of exactly `max_bytes` bytes allocated once, written
//! circularly; the oldest records are evicted, and an export keeps whole sessions only. The ring
//! is saved with [`ClientCapture::save`] (atomic: a unique tmp file, then a rename) as a
//! `.netcapture` file ([`CAPTURE_MAGIC`], format v1, `docs/formats/netcapture.md`).
//!
//! A replay ([`NetClient::replay`]) is a client that reads a captured session instead of its
//! socket: each `receive` takes the records of the next captured receive, so the same decrypted
//! packets reach the same connection at the same milliseconds. Nothing is decrypted again; the
//! handshake is not run (the replay starts at the recorded accept). The replayed client's own
//! records, captured with a second tap, are compared with the capture by [`compare_sessions`]:
//! the receive and flush times, the input stamps and bytes, the end, and the game's frame digests.
//! A game that predicts (the soak client, an ECS world through `NetLink`) gets its prediction
//! corrections and interpolation output back from the same packets, which is what the frame
//! digests check.
//!
//! Original design on the public sources of spec 103 § 13 and spec 120 § 19 (the record as the
//! client's stream: Valve Developer Community, "Source Multiplayer Networking").
//!
//! [`NetClient`]: crate::NetClient
//! [`NetClient::replay`]: crate::NetClient::replay

use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::snapshot_delta::crc32_ieee;

/// The first eight bytes of a `.netcapture` file.
pub const CAPTURE_MAGIC: [u8; 8] = *b"RNETCAPT";
/// The current `.netcapture` format version.
pub const CAPTURE_FORMAT_VERSION: u32 = 1;
/// The oldest version this build reads.
pub const CAPTURE_OLDEST_FORMAT_VERSION: u32 = 1;
/// Default ring size: 16 MiB, about a minute of a busy client's traffic.
pub const DEFAULT_CAPTURE_RING_BYTES: usize = 16 << 20;
/// Largest ring a capture may ask for (1 GiB); more is refused with `capture_config_invalid`.
pub const MAX_CAPTURE_RING_BYTES: usize = 1 << 30;
/// Smallest ring (4 KiB).
pub const MIN_CAPTURE_RING_BYTES: usize = 4 << 10;
/// Largest record payload a reader accepts (1 MiB); checked against the bytes left first.
pub const MAX_CAPTURE_RECORD_BYTES: usize = 1 << 20;
/// Bytes a record costs in the ring besides its payload: kind (1) and length (4).
pub const CAPTURE_RECORD_HEADER_BYTES: usize = 5;
/// Largest `.netcapture` header body (64 KiB).
pub const MAX_CAPTURE_HEADER_BYTES: usize = 64 << 10;

const KIND_SESSION: u8 = 1;
const KIND_RECEIVE: u8 = 2;
const KIND_FLUSH: u8 = 3;
const KIND_ACCEPT: u8 = 4;
const KIND_DENY: u8 = 5;
const KIND_PACKET: u8 = 6;
const KIND_GOODBYE: u8 = 7;
const KIND_ENDED: u8 = 8;
const KIND_INPUT: u8 = 9;
const KIND_FRAME: u8 = 10;
const KIND_HANDSHAKE: u8 = 11;
const KIND_END_OF_FILE: u8 = 0xEE;

/// One captured event (module docs). Times are the client's milliseconds (the value its
/// `receive` and `flush` got: a link clock, virtual in tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureRecord {
    /// A client started with the tap attached: one connection attempt.
    Session {
        /// `NetGame::GAME_ID`.
        game: String,
        /// `protocol_id::<G>()`: a replay of another protocol is refused.
        protocol: u64,
    },
    /// `NetClient::receive` at this time.
    Receive {
        /// The client's milliseconds.
        at_ms: u64,
    },
    /// `NetClient::flush` at this time.
    Flush {
        /// The client's milliseconds.
        at_ms: u64,
    },
    /// The server's accept (the `Accept` datagram, re-encoded after decryption).
    Accept {
        /// The link was sealed (the sealed header size sets how much fits in a packet).
        sealed: bool,
        /// `handshake::Accept::to_datagram()`.
        datagram: Vec<u8>,
    },
    /// The server's refusal (`handshake::Deny::to_datagram()`).
    Deny {
        /// The datagram.
        datagram: Vec<u8>,
    },
    /// An authenticated connection packet, after decryption and the token check.
    Packet {
        /// The packet the connection received.
        bytes: Vec<u8>,
    },
    /// The server said goodbye.
    Goodbye {
        /// Its reason byte, if it sent one.
        reason: Option<u8>,
    },
    /// The client stopped (`ClientEnd::as_str`).
    Ended {
        /// The end code.
        code: String,
    },
    /// `NetClient::send_input`: the stamp and the encoded input.
    Input {
        /// The tick the input is stamped for.
        tick: u64,
        /// `NetCodec::to_bytes` of the input.
        bytes: Vec<u8>,
    },
    /// The game's frame marker ([`CaptureTap::frame`]).
    Frame {
        /// The game's time for the frame (any unit the game picks; the soak uses virtual ms).
        at_ms: u64,
        /// The game's digest of its output so far (corrections, predicted and shown state).
        digest: u64,
    },
    /// The handshake's shortest round trip, written just before the accept it led to
    /// (ADR-10231). The client anchors its clock and its startup lead with it (ADR-10096); a
    /// replay runs no handshake and takes it from here. A capture without one (a session whose
    /// handshake measured no round trip) replays with none.
    Handshake {
        /// The round trip in the client's milliseconds.
        rtt_ms: u64,
    },
}

impl CaptureRecord {
    fn kind(&self) -> u8 {
        match self {
            Self::Session { .. } => KIND_SESSION,
            Self::Receive { .. } => KIND_RECEIVE,
            Self::Flush { .. } => KIND_FLUSH,
            Self::Accept { .. } => KIND_ACCEPT,
            Self::Deny { .. } => KIND_DENY,
            Self::Packet { .. } => KIND_PACKET,
            Self::Goodbye { .. } => KIND_GOODBYE,
            Self::Ended { .. } => KIND_ENDED,
            Self::Input { .. } => KIND_INPUT,
            Self::Frame { .. } => KIND_FRAME,
            Self::Handshake { .. } => KIND_HANDSHAKE,
        }
    }

    /// Lowercase name of the record kind.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Session { .. } => "session",
            Self::Receive { .. } => "receive",
            Self::Flush { .. } => "flush",
            Self::Accept { .. } => "accept",
            Self::Deny { .. } => "deny",
            Self::Packet { .. } => "packet",
            Self::Goodbye { .. } => "goodbye",
            Self::Ended { .. } => "ended",
            Self::Input { .. } => "input",
            Self::Frame { .. } => "frame",
            Self::Handshake { .. } => "handshake",
        }
    }

    fn payload(&self) -> Vec<u8> {
        let mut p = Vec::new();
        match self {
            Self::Session { game, protocol } => {
                let g = game.as_bytes();
                let n = g.len().min(usize::from(u16::MAX));
                p.extend_from_slice(&(n as u16).to_le_bytes());
                p.extend_from_slice(&g[..n]);
                p.extend_from_slice(&protocol.to_le_bytes());
            }
            Self::Receive { at_ms } | Self::Flush { at_ms } => p.extend_from_slice(&at_ms.to_le_bytes()),
            Self::Accept { sealed, datagram } => {
                p.push(u8::from(*sealed));
                p.extend_from_slice(datagram);
            }
            Self::Deny { datagram } => p.extend_from_slice(datagram),
            Self::Packet { bytes } => p.extend_from_slice(bytes),
            Self::Goodbye { reason } => {
                p.push(u8::from(reason.is_some()));
                p.push(reason.unwrap_or(0));
            }
            Self::Ended { code } => p.extend_from_slice(code.as_bytes()),
            Self::Input { tick, bytes } => {
                p.extend_from_slice(&tick.to_le_bytes());
                p.extend_from_slice(bytes);
            }
            Self::Frame { at_ms, digest } => {
                p.extend_from_slice(&at_ms.to_le_bytes());
                p.extend_from_slice(&digest.to_le_bytes());
            }
            Self::Handshake { rtt_ms } => p.extend_from_slice(&rtt_ms.to_le_bytes()),
        }
        p
    }

    fn decode(kind: u8, p: &[u8]) -> Result<Self, CaptureError> {
        let bad = || CaptureError::Corrupt(format!("record kind {kind} of {} bytes", p.len()));
        let u64_at = |at: usize| -> Result<u64, CaptureError> {
            p.get(at..at + 8)
                .and_then(|b| b.try_into().ok())
                .map(u64::from_le_bytes)
                .ok_or_else(bad)
        };
        Ok(match kind {
            KIND_SESSION => {
                let n = p
                    .get(..2)
                    .and_then(|b| b.try_into().ok())
                    .map(u16::from_le_bytes)
                    .ok_or_else(bad)? as usize;
                let game = p.get(2..2 + n).ok_or_else(bad)?;
                let game = std::str::from_utf8(game).map_err(|_| bad())?.to_owned();
                if p.len() != 2 + n + 8 {
                    return Err(bad());
                }
                Self::Session {
                    game,
                    protocol: u64_at(2 + n)?,
                }
            }
            KIND_RECEIVE | KIND_FLUSH => {
                if p.len() != 8 {
                    return Err(bad());
                }
                let at_ms = u64_at(0)?;
                if kind == KIND_RECEIVE {
                    Self::Receive { at_ms }
                } else {
                    Self::Flush { at_ms }
                }
            }
            KIND_ACCEPT => {
                let (&s, rest) = p.split_first().ok_or_else(bad)?;
                if s > 1 {
                    return Err(bad());
                }
                Self::Accept {
                    sealed: s == 1,
                    datagram: rest.to_vec(),
                }
            }
            KIND_DENY => Self::Deny { datagram: p.to_vec() },
            KIND_PACKET => Self::Packet { bytes: p.to_vec() },
            KIND_GOODBYE => match p {
                [0, 0] => Self::Goodbye { reason: None },
                [1, r] => Self::Goodbye { reason: Some(*r) },
                _ => return Err(bad()),
            },
            KIND_ENDED => Self::Ended {
                code: std::str::from_utf8(p).map_err(|_| bad())?.to_owned(),
            },
            KIND_INPUT => Self::Input {
                tick: u64_at(0)?,
                bytes: p.get(8..).ok_or_else(bad)?.to_vec(),
            },
            KIND_FRAME => {
                if p.len() != 16 {
                    return Err(bad());
                }
                Self::Frame {
                    at_ms: u64_at(0)?,
                    digest: u64_at(8)?,
                }
            }
            KIND_HANDSHAKE => {
                if p.len() != 8 {
                    return Err(bad());
                }
                Self::Handshake { rtt_ms: u64_at(0)? }
            }
            _ => return Err(CaptureError::Corrupt(format!("unknown record kind {kind}"))),
        })
    }

    /// Bytes this record takes in the ring (payload plus [`CAPTURE_RECORD_HEADER_BYTES`]).
    pub fn ring_bytes(&self) -> usize {
        self.payload().len() + CAPTURE_RECORD_HEADER_BYTES
    }
}

/// Why a capture could not be made, saved, read or replayed. [`CaptureError::code`] is the
/// agent-protocol code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureError {
    /// The ring size is outside [`MIN_CAPTURE_RING_BYTES`]..=[`MAX_CAPTURE_RING_BYTES`].
    ConfigInvalid(String),
    /// A file error (the message names the path).
    Io(String),
    /// The file exists and `overwrite` is off.
    Exists(PathBuf),
    /// The bytes are not a `.netcapture`.
    NotACapture,
    /// A `format_version` this build does not read.
    Version(u32),
    /// The file ends early.
    Truncated,
    /// A checksum, a length or a record is wrong.
    Corrupt(String),
    /// The capture holds no whole session (empty, or its start was evicted).
    Empty,
    /// The capture was made by another game or protocol than the replaying client.
    ProtocolMismatch {
        /// The capture's game and protocol id.
        captured: String,
        /// The replaying client's.
        replaying: String,
    },
}

impl CaptureError {
    /// Lowercase agent-protocol code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::ConfigInvalid(_) => "capture_config_invalid",
            Self::Io(_) => "capture_io",
            Self::Exists(_) => "capture_exists",
            Self::NotACapture => "capture_not_a_capture",
            Self::Version(_) => "capture_version",
            Self::Truncated => "capture_truncated",
            Self::Corrupt(_) => "capture_corrupt",
            Self::Empty => "capture_empty",
            Self::ProtocolMismatch { .. } => "capture_protocol_mismatch",
        }
    }
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfigInvalid(m) => write!(f, "{}: {m}", self.code()),
            Self::Io(m) => write!(f, "{}: {m}", self.code()),
            Self::Exists(p) => write!(f, "{}: {} exists", self.code(), p.display()),
            Self::NotACapture | Self::Truncated | Self::Empty => f.write_str(self.code()),
            Self::Version(v) => write!(
                f,
                "{}: format version {v}, this build reads {CAPTURE_OLDEST_FORMAT_VERSION}..={CAPTURE_FORMAT_VERSION}",
                self.code()
            ),
            Self::Corrupt(m) => write!(f, "{}: {m}", self.code()),
            Self::ProtocolMismatch { captured, replaying } => {
                write!(f, "{}: captured {captured}, replaying {replaying}", self.code())
            }
        }
    }
}

impl std::error::Error for CaptureError {}

/// Counters of a [`ClientCapture`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureStats {
    /// The ring's size, bytes (allocated once, never grown).
    pub ring_bytes: usize,
    /// Bytes the records in the ring take now.
    pub used_bytes: usize,
    /// The most `used_bytes` ever reached.
    pub peak_used_bytes: usize,
    /// Records in the ring.
    pub records: u64,
    /// Records written since the start.
    pub written: u64,
    /// Records evicted to make room.
    pub evicted: u64,
    /// Records larger than the whole ring, dropped (the ring then waits for the next session).
    pub oversized: u64,
    /// Whole sessions in the ring (the first one may have lost its start; see `head_cut`).
    pub sessions: u64,
    /// The oldest record in the ring is not a session start (eviction cut into a session).
    pub head_cut: bool,
}

/// A client capture: a fixed byte ring of [`CaptureRecord`]s (module docs).
pub struct ClientCapture {
    ring: Vec<u8>,
    /// Offset of the oldest record.
    head: usize,
    /// Bytes in use from `head`, wrapping.
    used: usize,
    records: u64,
    sessions: u64,
    head_cut: bool,
    /// After an oversized record the ring drops records until the next session.
    waiting_for_session: bool,
    peak: usize,
    written: u64,
    evicted: u64,
    oversized: u64,
}

impl std::fmt::Debug for ClientCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientCapture")
            .field("stats", &self.stats())
            .finish()
    }
}

impl ClientCapture {
    /// A capture whose ring holds `max_bytes` bytes, allocated now.
    ///
    /// # Errors
    ///
    /// `capture_config_invalid` outside [`MIN_CAPTURE_RING_BYTES`]..=[`MAX_CAPTURE_RING_BYTES`], or
    /// when the memory is not there.
    pub fn new(max_bytes: usize) -> Result<Self, CaptureError> {
        if !(MIN_CAPTURE_RING_BYTES..=MAX_CAPTURE_RING_BYTES).contains(&max_bytes) {
            return Err(CaptureError::ConfigInvalid(format!(
                "ring of {max_bytes} bytes; allowed {MIN_CAPTURE_RING_BYTES}..={MAX_CAPTURE_RING_BYTES}"
            )));
        }
        let mut ring = Vec::new();
        ring.try_reserve_exact(max_bytes)
            .map_err(|e| CaptureError::ConfigInvalid(format!("ring of {max_bytes} bytes: {e}")))?;
        ring.resize(max_bytes, 0);
        Ok(Self {
            ring,
            head: 0,
            used: 0,
            records: 0,
            sessions: 0,
            head_cut: false,
            waiting_for_session: false,
            peak: 0,
            written: 0,
            evicted: 0,
            oversized: 0,
        })
    }

    /// The counters.
    pub fn stats(&self) -> CaptureStats {
        CaptureStats {
            ring_bytes: self.ring.len(),
            used_bytes: self.used,
            peak_used_bytes: self.peak,
            records: self.records,
            written: self.written,
            evicted: self.evicted,
            oversized: self.oversized,
            sessions: self.sessions,
            head_cut: self.head_cut,
        }
    }

    /// Heap bytes the ring holds (its capacity): fixed at [`ClientCapture::new`].
    pub fn heap_bytes(&self) -> usize {
        self.ring.capacity()
    }

    fn cap(&self) -> usize {
        self.ring.len()
    }

    fn write_at(&mut self, at: usize, bytes: &[u8]) {
        let cap = self.cap();
        let first = bytes.len().min(cap - at);
        self.ring[at..at + first].copy_from_slice(&bytes[..first]);
        self.ring[..bytes.len() - first].copy_from_slice(&bytes[first..]);
    }

    fn read_at(&self, at: usize, out: &mut [u8]) {
        let cap = self.cap();
        let first = out.len().min(cap - at);
        out[..first].copy_from_slice(&self.ring[at..at + first]);
        let rest = out.len() - first;
        out[first..].copy_from_slice(&self.ring[..rest]);
    }

    /// `(kind, payload length)` of the record at `at`.
    fn header_at(&self, at: usize) -> (u8, usize) {
        let mut h = [0u8; CAPTURE_RECORD_HEADER_BYTES];
        self.read_at(at, &mut h);
        (h[0], u32::from_le_bytes([h[1], h[2], h[3], h[4]]) as usize)
    }

    fn evict_one(&mut self) {
        let (kind, len) = self.header_at(self.head);
        let size = CAPTURE_RECORD_HEADER_BYTES + len;
        self.head = (self.head + size) % self.cap();
        self.used -= size;
        self.records -= 1;
        self.evicted += 1;
        if kind == KIND_SESSION {
            self.sessions = self.sessions.saturating_sub(1);
        }
        self.head_cut = self.records > 0 && self.header_at(self.head).0 != KIND_SESSION;
    }

    /// Appends a record, evicting the oldest ones when the ring is full.
    pub fn push(&mut self, record: &CaptureRecord) {
        self.written += 1;
        let kind = record.kind();
        if self.waiting_for_session && kind != KIND_SESSION {
            self.oversized += 1;
            return;
        }
        let payload = record.payload();
        let size = CAPTURE_RECORD_HEADER_BYTES + payload.len();
        if size > self.cap() || payload.len() > MAX_CAPTURE_RECORD_BYTES {
            // It cannot fit: the session it belongs to can no longer replay.
            self.oversized += 1;
            self.clear();
            self.waiting_for_session = true;
            return;
        }
        self.waiting_for_session = false;
        while self.used + size > self.cap() {
            self.evict_one();
        }
        let at = (self.head + self.used) % self.cap();
        let mut h = [0u8; CAPTURE_RECORD_HEADER_BYTES];
        h[0] = kind;
        h[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        self.write_at(at, &h);
        self.write_at((at + CAPTURE_RECORD_HEADER_BYTES) % self.cap(), &payload);
        self.used += size;
        self.records += 1;
        if kind == KIND_SESSION {
            self.sessions += 1;
        }
        self.peak = self.peak.max(self.used);
    }

    /// Empties the ring (its memory stays).
    pub fn clear(&mut self) {
        self.head = 0;
        self.used = 0;
        self.records = 0;
        self.sessions = 0;
        self.head_cut = false;
    }

    /// Every record in the ring, oldest first.
    pub fn records(&self) -> Vec<CaptureRecord> {
        let mut out = Vec::with_capacity(self.records as usize);
        let mut at = self.head;
        let mut left = self.used;
        while left >= CAPTURE_RECORD_HEADER_BYTES {
            let (kind, len) = self.header_at(at);
            let mut payload = vec![0u8; len];
            self.read_at((at + CAPTURE_RECORD_HEADER_BYTES) % self.cap(), &mut payload);
            // The ring only holds records `push` encoded, so a decode cannot fail; skip if it did.
            if let Ok(r) = CaptureRecord::decode(kind, &payload) {
                out.push(r);
            }
            let size = CAPTURE_RECORD_HEADER_BYTES + len;
            at = (at + size) % self.cap();
            left -= size;
        }
        out
    }

    /// The whole sessions in the ring: each starts at a [`CaptureRecord::Session`]. Records
    /// before the first session start (a session whose start was evicted) are left out.
    pub fn sessions(&self) -> Vec<Vec<CaptureRecord>> {
        split_sessions(&self.records())
    }

    /// Writes the whole sessions of the ring to `path` as a `.netcapture` (module docs): a unique
    /// tmp file next to it, synced, then renamed over `path`. `label` goes in the header (the
    /// soak names the run and client there).
    ///
    /// # Errors
    ///
    /// `capture_exists` when `path` exists and `overwrite` is off; `capture_empty` when the ring
    /// holds no whole session; `capture_io` on a file error (the tmp file is removed).
    pub fn save(&self, path: &Path, label: &str, overwrite: bool) -> Result<CaptureSummary, CaptureError> {
        let sessions = self.sessions();
        if sessions.is_empty() {
            return Err(CaptureError::Empty);
        }
        let records: Vec<CaptureRecord> = sessions.into_iter().flatten().collect();
        let file = CaptureFile {
            label: label.to_owned(),
            stats: self.stats(),
            records,
        };
        file.save(path, overwrite)
    }
}

/// Splits records into sessions (each from a [`CaptureRecord::Session`] to the next).
pub fn split_sessions(records: &[CaptureRecord]) -> Vec<Vec<CaptureRecord>> {
    let mut out: Vec<Vec<CaptureRecord>> = Vec::new();
    for r in records {
        match r {
            CaptureRecord::Session { .. } => out.push(vec![r.clone()]),
            _ => {
                if let Some(s) = out.last_mut() {
                    s.push(r.clone());
                }
            }
        }
    }
    out
}

/// What a saved capture holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureSummary {
    /// Where it is.
    pub path: PathBuf,
    /// Bytes written.
    pub bytes: u64,
    /// Records written.
    pub records: u64,
    /// Sessions written.
    pub sessions: u64,
}

/// A `.netcapture` file in memory: its label, the ring's counters at the save, and its records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureFile {
    /// The label (free text: the soak writes `profile/clients/mode seed=… client=…`).
    pub label: String,
    /// The ring's counters when it was saved.
    pub stats: CaptureStats,
    /// The records, oldest first, whole sessions only.
    pub records: Vec<CaptureRecord>,
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl CaptureFile {
    /// The sessions of the file.
    pub fn sessions(&self) -> Vec<Vec<CaptureRecord>> {
        split_sessions(&self.records)
    }

    /// The file's bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut body = Vec::new();
        let label = self.label.as_bytes();
        let n = label.len().min(MAX_CAPTURE_HEADER_BYTES - 128);
        body.extend_from_slice(&(n as u32).to_le_bytes());
        body.extend_from_slice(&label[..n]);
        let s = &self.stats;
        for v in [
            s.ring_bytes as u64,
            s.peak_used_bytes as u64,
            s.written,
            s.evicted,
            s.oversized,
        ] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        let mut out = Vec::new();
        out.extend_from_slice(&CAPTURE_MAGIC);
        out.extend_from_slice(&CAPTURE_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&crc32_ieee(&body).to_le_bytes());
        out.extend_from_slice(&body);
        let mut push = |kind: u8, payload: &[u8]| {
            out.push(kind);
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            let mut c = vec![kind];
            c.extend_from_slice(payload);
            out.extend_from_slice(&crc32_ieee(&c).to_le_bytes());
            out.extend_from_slice(payload);
        };
        for r in &self.records {
            push(r.kind(), &r.payload());
        }
        push(KIND_END_OF_FILE, &(self.records.len() as u64).to_le_bytes());
        out
    }

    /// Reads a `.netcapture` from its bytes. Every length is checked against the bytes left before
    /// anything is allocated; every record's CRC-32 is checked; the end record's count must match.
    ///
    /// # Errors
    ///
    /// `capture_not_a_capture`, `capture_version`, `capture_truncated` or `capture_corrupt`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CaptureError> {
        let mut r = Cursor { bytes, at: 0 };
        let magic = r.take(8).map_err(|_| CaptureError::NotACapture)?;
        if magic != CAPTURE_MAGIC {
            return Err(CaptureError::NotACapture);
        }
        let version = r.u32()?;
        if !(CAPTURE_OLDEST_FORMAT_VERSION..=CAPTURE_FORMAT_VERSION).contains(&version) {
            return Err(CaptureError::Version(version));
        }
        let body_len = r.u32()? as usize;
        if body_len > MAX_CAPTURE_HEADER_BYTES {
            return Err(CaptureError::Corrupt(format!("header of {body_len} bytes")));
        }
        let crc = r.u32()?;
        let body = r.take(body_len)?;
        if crc32_ieee(body) != crc {
            return Err(CaptureError::Corrupt("header checksum".into()));
        }
        let mut h = Cursor { bytes: body, at: 0 };
        let n = h.u32()? as usize;
        let label = std::str::from_utf8(h.take(n)?)
            .map_err(|_| CaptureError::Corrupt("label is not UTF-8".into()))?
            .to_owned();
        let mut v = [0u64; 5];
        for x in &mut v {
            *x = h.u64()?;
        }
        if h.at != body.len() {
            return Err(CaptureError::Corrupt("header has trailing bytes".into()));
        }
        let stats = CaptureStats {
            ring_bytes: usize::try_from(v[0]).unwrap_or(usize::MAX),
            peak_used_bytes: usize::try_from(v[1]).unwrap_or(usize::MAX),
            written: v[2],
            evicted: v[3],
            oversized: v[4],
            ..CaptureStats::default()
        };
        let mut records = Vec::new();
        loop {
            let kind = r.u8()?;
            let len = r.u32()? as usize;
            if len > MAX_CAPTURE_RECORD_BYTES {
                return Err(CaptureError::Corrupt(format!("record of {len} bytes")));
            }
            let crc = r.u32()?;
            let payload = r.take(len)?;
            let mut c = Vec::with_capacity(len + 1);
            c.push(kind);
            c.extend_from_slice(payload);
            if crc32_ieee(&c) != crc {
                return Err(CaptureError::Corrupt(format!(
                    "record {} checksum",
                    records.len()
                )));
            }
            if kind == KIND_END_OF_FILE {
                let count = Cursor {
                    bytes: payload,
                    at: 0,
                }
                .u64()?;
                if count != records.len() as u64 || len != 8 {
                    return Err(CaptureError::Corrupt("end record count".into()));
                }
                if r.at != bytes.len() {
                    return Err(CaptureError::Corrupt("bytes after the end record".into()));
                }
                break;
            }
            let rec = CaptureRecord::decode(kind, payload)?;
            if records.is_empty() && !matches!(rec, CaptureRecord::Session { .. }) {
                return Err(CaptureError::Corrupt("the first record is not a session".into()));
            }
            records.push(rec);
        }
        let mut stats = stats;
        stats.records = records.len() as u64;
        stats.sessions = records
            .iter()
            .filter(|r| matches!(r, CaptureRecord::Session { .. }))
            .count() as u64;
        Ok(Self {
            label,
            stats,
            records,
        })
    }

    /// Writes the file atomically (module docs).
    ///
    /// # Errors
    ///
    /// `capture_exists` or `capture_io`.
    pub fn save(&self, path: &Path, overwrite: bool) -> Result<CaptureSummary, CaptureError> {
        if !overwrite && path.exists() {
            return Err(CaptureError::Exists(path.to_path_buf()));
        }
        let bytes = self.to_bytes();
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = path
            .file_name()
            .map_or_else(|| "capture".into(), |s| s.to_string_lossy().into_owned());
        let tmp = path.with_file_name(format!("{name}.{}.{n}.tmp", std::process::id()));
        let io = |e: std::io::Error, p: &Path| CaptureError::Io(format!("{}: {e}", p.display()));
        let written = (|| -> std::io::Result<()> {
            let mut f = File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(io(e, &tmp));
        }
        if !overwrite && path.exists() {
            let _ = std::fs::remove_file(&tmp);
            return Err(CaptureError::Exists(path.to_path_buf()));
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(io(e, path));
        }
        Ok(CaptureSummary {
            path: path.to_path_buf(),
            bytes: bytes.len() as u64,
            records: self.records.len() as u64,
            sessions: self.stats.sessions.max(self.sessions().len() as u64),
        })
    }

    /// Reads a `.netcapture` file.
    ///
    /// # Errors
    ///
    /// `capture_io` or any error of [`CaptureFile::from_bytes`].
    pub fn load(path: &Path) -> Result<Self, CaptureError> {
        let bytes = std::fs::read(path).map_err(|e| CaptureError::Io(format!("{}: {e}", path.display())))?;
        Self::from_bytes(&bytes)
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], CaptureError> {
        let end = self.at.checked_add(n).ok_or(CaptureError::Truncated)?;
        let s = self.bytes.get(self.at..end).ok_or(CaptureError::Truncated)?;
        self.at = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, CaptureError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, CaptureError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Result<u64, CaptureError> {
        let mut a = [0u8; 8];
        a.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(a))
    }
}

/// A shared handle on a [`ClientCapture`]: the client writes through it, the game reads and
/// saves through another clone. Cloning shares the ring.
#[derive(Clone)]
pub struct CaptureTap(Arc<Mutex<ClientCapture>>);

impl std::fmt::Debug for CaptureTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CaptureTap").field(&self.stats()).finish()
    }
}

impl CaptureTap {
    /// A tap on a new ring of `max_bytes` bytes.
    ///
    /// # Errors
    ///
    /// As [`ClientCapture::new`].
    pub fn new(max_bytes: usize) -> Result<Self, CaptureError> {
        Ok(Self(Arc::new(Mutex::new(ClientCapture::new(max_bytes)?))))
    }

    /// The ring, locked.
    pub fn lock(&self) -> MutexGuard<'_, ClientCapture> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Appends a record.
    pub fn push(&self, record: CaptureRecord) {
        self.lock().push(&record);
    }

    /// The game's frame marker: its time and a digest of what it showed and predicted so far.
    pub fn frame(&self, at_ms: u64, digest: u64) {
        self.push(CaptureRecord::Frame { at_ms, digest });
    }

    /// The ring's counters.
    pub fn stats(&self) -> CaptureStats {
        self.lock().stats()
    }

    /// See [`ClientCapture::save`].
    ///
    /// # Errors
    ///
    /// As [`ClientCapture::save`].
    pub fn save(&self, path: &Path, label: &str, overwrite: bool) -> Result<CaptureSummary, CaptureError> {
        self.lock().save(path, label, overwrite)
    }
}

/// A captured session a replaying client reads (made by [`ReplayFeed::new`]).
#[derive(Debug, Clone, Default)]
pub struct ReplayFeed {
    pub(crate) records: VecDeque<CaptureRecord>,
    /// Receive calls whose captured time differed from the replay's.
    pub(crate) time_mismatches: u64,
    /// Receive calls with no captured receive left.
    pub(crate) overruns: u64,
}

impl ReplayFeed {
    /// The feed of one session (its records from its [`CaptureRecord::Session`]).
    ///
    /// # Errors
    ///
    /// `capture_empty` when the session does not start with a session record.
    pub fn new(session: Vec<CaptureRecord>) -> Result<Self, CaptureError> {
        if !matches!(session.first(), Some(CaptureRecord::Session { .. })) {
            return Err(CaptureError::Empty);
        }
        Ok(Self {
            records: session.into(),
            time_mismatches: 0,
            overruns: 0,
        })
    }

    /// The session record's game and protocol.
    pub fn protocol(&self) -> Option<(&str, u64)> {
        match self.records.front() {
            Some(CaptureRecord::Session { game, protocol }) => Some((game, *protocol)),
            _ => None,
        }
    }

    /// Receive calls whose time differed from the captured one.
    pub fn time_mismatches(&self) -> u64 {
        self.time_mismatches
    }

    /// Receive calls after the captured ones ran out.
    pub fn overruns(&self) -> u64 {
        self.overruns
    }

    /// Captured records not reached yet.
    pub fn remaining(&self) -> usize {
        self.records.len()
    }

    /// The three counters at once.
    pub fn progress(&self) -> ReplayProgress {
        ReplayProgress {
            time_mismatches: self.time_mismatches,
            overruns: self.overruns,
            remaining: self.records.len(),
        }
    }
}

/// How far a replay got ([`ReplayFeed::progress`], `NetLink::replay_progress`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReplayProgress {
    /// Receive calls whose time differed from the captured one (0 when the replay runs at the
    /// captured times).
    pub time_mismatches: u64,
    /// Receive calls after the captured ones ran out.
    pub overruns: u64,
    /// Captured records not reached yet.
    pub remaining: usize,
}

/// A socket for a replaying client: it receives nothing (the capture feeds the client) and drops
/// what it is given. `max_datagram_size` is the captured transport's, so the replay cuts its
/// packets as the capture did.
#[derive(Debug, Clone, Copy)]
pub struct ReplaySocket {
    max_datagram: Option<usize>,
}

impl ReplaySocket {
    /// A socket with the default datagram size of [`crate::socket::DatagramSocket`]
    /// ([`crate::socket::MAX_DATAGRAM_BYTES`], the in-memory network's).
    pub fn new() -> Self {
        Self { max_datagram: None }
    }

    /// A socket that reports `bytes` as its largest datagram (a Steam or relay transport's size).
    pub fn with_max_datagram(bytes: usize) -> Self {
        Self {
            max_datagram: Some(bytes),
        }
    }
}

impl Default for ReplaySocket {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::socket::DatagramSocket for ReplaySocket {
    fn send_to(&mut self, _bytes: &[u8], _dest: std::net::SocketAddr) -> std::io::Result<()> {
        Ok(())
    }

    fn recv_from(&mut self, _buf: &mut [u8]) -> std::io::Result<Option<(usize, std::net::SocketAddr)>> {
        Ok(None)
    }

    fn max_datagram_size(&self, _dest: std::net::SocketAddr) -> usize {
        self.max_datagram.unwrap_or(crate::socket::MAX_DATAGRAM_BYTES)
    }

    fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        Ok(std::net::SocketAddr::from(([0, 0, 0, 0], 0)))
    }
}

/// The first difference between a captured session and its replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureDivergence {
    /// Index of the record in the compared streams (packets and session records left out).
    pub index: usize,
    /// The captured record (`None` when the capture ended first).
    pub captured: Option<String>,
    /// The replayed record (`None` when the replay ended first).
    pub replayed: Option<String>,
}

fn short(r: &CaptureRecord) -> String {
    match r {
        CaptureRecord::Input { tick, bytes } => format!("input tick {tick} ({} bytes)", bytes.len()),
        CaptureRecord::Packet { bytes } => format!("packet ({} bytes)", bytes.len()),
        CaptureRecord::Accept { sealed, .. } => format!("accept sealed={sealed}"),
        CaptureRecord::Deny { .. } => "deny".into(),
        other => format!("{other:?}"),
    }
}

/// Compares a captured session with the records a replay of it wrote: every record but the
/// packets and the server's answers (the replay was fed those) must be equal, in order: receive
/// and flush times, input stamps and bytes, the end, and the game's frame markers with their
/// digests. `None` when they match.
pub fn compare_sessions(captured: &[CaptureRecord], replayed: &[CaptureRecord]) -> Option<CaptureDivergence> {
    let own = |r: &&CaptureRecord| {
        !matches!(
            r,
            CaptureRecord::Packet { .. }
                | CaptureRecord::Accept { .. }
                | CaptureRecord::Deny { .. }
                | CaptureRecord::Goodbye { .. }
                | CaptureRecord::Session { .. }
                | CaptureRecord::Handshake { .. }
        )
    };
    let a: Vec<&CaptureRecord> = captured.iter().filter(own).collect();
    let b: Vec<&CaptureRecord> = replayed.iter().filter(own).collect();
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i), b.get(i));
        if x != y {
            return Some(CaptureDivergence {
                index: i,
                captured: x.map(|r| short(r)),
                replayed: y.map(|r| short(r)),
            });
        }
    }
    None
}

/// The schema of `.netcapture` for `cargo xtask check-formats` (spec 12 § 4a).
#[must_use]
pub fn netcapture_schema_fingerprint() -> String {
    concat!(
        "preamble:magic,format_version,body_len,body_crc;",
        "body:label,ring_bytes,peak_used_bytes,written,evicted,oversized;",
        "record:kind,len,crc,payload;kinds:1=session[game,protocol],2=receive[at_ms],3=flush[at_ms],",
        "4=accept[sealed,datagram],5=deny[datagram],6=packet[bytes],7=goodbye[has,reason],8=ended[code],",
        "9=input[tick,bytes],10=frame[at_ms,digest],11=handshake[rtt_ms],238=end[records]",
    )
    .to_string()
}

/// The `from` versions of the `.netcapture` migrations: none, the format has one version.
#[must_use]
pub fn netcapture_migration_sources() -> Vec<u32> {
    (CAPTURE_OLDEST_FORMAT_VERSION..CAPTURE_FORMAT_VERSION).collect()
}

/// The `format_version` of a `.netcapture`'s bytes, for `regia upgrade-project`: `Ok(None)` when
/// they are not one.
///
/// # Errors
///
/// A reason when the magic is there and the version is cut.
pub fn netcapture_file_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    if bytes.get(..CAPTURE_MAGIC.len()) != Some(&CAPTURE_MAGIC[..]) {
        return Ok(None);
    }
    let Some(v) = bytes.get(CAPTURE_MAGIC.len()..CAPTURE_MAGIC.len() + 4) else {
        return Err("the format version is cut".into());
    };
    Ok(Some(u32::from_le_bytes([v[0], v[1], v[2], v[3]])))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(n: u64) -> Vec<CaptureRecord> {
        let mut v = vec![CaptureRecord::Session {
            game: "test.capture".into(),
            protocol: 7,
        }];
        for i in 0..n {
            v.push(CaptureRecord::Receive { at_ms: i * 16 });
            v.push(CaptureRecord::Packet {
                bytes: vec![i as u8; 40 + (i % 7) as usize],
            });
            v.push(CaptureRecord::Input {
                tick: i,
                bytes: vec![1, 2, 3],
            });
            v.push(CaptureRecord::Flush { at_ms: i * 16 });
            v.push(CaptureRecord::Frame {
                at_ms: i * 16,
                digest: i.wrapping_mul(0x9E37_79B9),
            });
        }
        v
    }

    #[test]
    fn every_record_round_trips() {
        let all = vec![
            CaptureRecord::Session {
                game: "g".into(),
                protocol: u64::MAX,
            },
            CaptureRecord::Receive { at_ms: 5 },
            CaptureRecord::Flush { at_ms: 6 },
            CaptureRecord::Accept {
                sealed: true,
                datagram: vec![9; 20],
            },
            CaptureRecord::Deny { datagram: vec![1] },
            CaptureRecord::Packet { bytes: vec![] },
            CaptureRecord::Goodbye { reason: None },
            CaptureRecord::Goodbye { reason: Some(3) },
            CaptureRecord::Ended {
                code: "timeout".into(),
            },
            CaptureRecord::Input {
                tick: 99,
                bytes: vec![4, 5],
            },
            CaptureRecord::Frame { at_ms: 1, digest: 2 },
        ];
        for r in &all {
            assert_eq!(
                &CaptureRecord::decode(r.kind(), &r.payload()).expect("decodes"),
                r
            );
        }
        let file = CaptureFile {
            label: "x".into(),
            stats: CaptureStats::default(),
            records: all.clone(),
        };
        let back = CaptureFile::from_bytes(&file.to_bytes()).expect("reads");
        assert_eq!(back.records, all);
        assert_eq!(back.label, "x");
    }

    #[test]
    fn the_ring_never_grows_and_keeps_the_newest_whole_sessions() {
        let mut c = ClientCapture::new(MIN_CAPTURE_RING_BYTES).expect("ring");
        let heap = c.heap_bytes();
        for s in 0..20u64 {
            // Sessions of 0.6-2.7 KiB in a 4 KiB ring: each fits whole, two never do.
            for r in session(5 + s) {
                c.push(&r);
                assert!(c.stats().used_bytes <= MIN_CAPTURE_RING_BYTES);
            }
        }
        assert_eq!(c.heap_bytes(), heap, "the ring is allocated once");
        let st = c.stats();
        assert!(st.evicted > 0 && st.peak_used_bytes <= MIN_CAPTURE_RING_BYTES);
        let sessions = c.sessions();
        assert!(!sessions.is_empty());
        assert_eq!(sessions.last().expect("newest"), &session(24));
    }

    #[test]
    fn an_oversized_record_drops_its_session_and_the_ring_recovers() {
        let mut c = ClientCapture::new(MIN_CAPTURE_RING_BYTES).expect("ring");
        for r in session(3) {
            c.push(&r);
        }
        c.push(&CaptureRecord::Packet {
            bytes: vec![0; MIN_CAPTURE_RING_BYTES],
        });
        c.push(&CaptureRecord::Receive { at_ms: 1 });
        assert_eq!(c.stats().records, 0);
        assert_eq!(c.stats().oversized, 2);
        for r in session(2) {
            c.push(&r);
        }
        assert_eq!(c.sessions(), vec![session(2)]);
    }

    #[test]
    fn a_ring_outside_the_bounds_is_refused() {
        for n in [0, MIN_CAPTURE_RING_BYTES - 1, MAX_CAPTURE_RING_BYTES + 1] {
            assert_eq!(
                ClientCapture::new(n).map(|_| ()).map_err(|e| e.code()),
                Err("capture_config_invalid")
            );
        }
    }

    #[test]
    fn every_truncation_and_bit_flip_is_a_typed_error_or_a_clean_read() {
        let file = CaptureFile {
            label: "t".into(),
            stats: CaptureStats::default(),
            records: session(4),
        };
        let bytes = file.to_bytes();
        for n in 0..bytes.len() {
            assert!(CaptureFile::from_bytes(&bytes[..n]).is_err(), "cut at {n}");
        }
        for i in 0..bytes.len() {
            for bit in 0..8 {
                let mut b = bytes.clone();
                b[i] ^= 1 << bit;
                let _ = CaptureFile::from_bytes(&b);
            }
        }
        let mut lying = bytes.clone();
        // The first record's length field claims 4 GiB: refused before any allocation.
        let at =
            8 + 4 + 4 + 4 + u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize + 1;
        lying[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            CaptureFile::from_bytes(&lying).map(|_| ()).map_err(|e| e.code()),
            Err("capture_corrupt")
        );
        assert_eq!(netcapture_file_version(&bytes), Ok(Some(CAPTURE_FORMAT_VERSION)));
        assert_eq!(netcapture_file_version(b"RNETDEMO"), Ok(None));
    }

    /// ADR-10231: the handshake's round trip is a record of its own (kind 11, eight bytes). It
    /// survives a save and a load, a wrong length is corrupt, and the comparison of a replay
    /// leaves it out, as it does the accept it comes with (the replay was fed both).
    #[test]
    fn the_handshake_round_trip_is_a_record_the_replay_is_fed() {
        let r = CaptureRecord::Handshake { rtt_ms: 137 };
        assert_eq!(r.kind_name(), "handshake");
        let payload = r.payload();
        assert_eq!(payload.len(), 8);
        assert_eq!(CaptureRecord::decode(r.kind(), &payload), Ok(r.clone()));
        assert_eq!(
            CaptureRecord::decode(r.kind(), &payload[..7]).map_err(|e| e.code()),
            Err("capture_corrupt")
        );
        let a = session(5);
        let mut b = a.clone();
        b.insert(1, r);
        assert_eq!(compare_sessions(&a, &b), None);
        assert_eq!(compare_sessions(&b, &a), None);
    }

    #[test]
    fn compare_names_the_first_difference_and_ignores_fed_records() {
        let a = session(5);
        let mut b = a.clone();
        b.retain(|r| !matches!(r, CaptureRecord::Packet { .. }));
        assert_eq!(compare_sessions(&a, &b), None);
        if let Some(CaptureRecord::Input { tick, .. }) = b
            .iter_mut()
            .find(|r| matches!(r, CaptureRecord::Input { tick: 3, .. }))
        {
            *tick = 4;
        }
        let d = compare_sessions(&a, &b).expect("differs");
        assert_eq!(d.captured.as_deref(), Some("input tick 3 (3 bytes)"));
    }

    #[test]
    fn save_is_atomic_and_refuses_an_existing_file() {
        let dir = std::env::temp_dir().join(format!(
            "regia-capture-{}-{}",
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let tap = CaptureTap::new(MIN_CAPTURE_RING_BYTES).expect("tap");
        let path = dir.join("c.netcapture");
        assert_eq!(
            tap.save(&path, "l", false).map(|_| ()).map_err(|e| e.code()),
            Err("capture_empty")
        );
        for r in session(3) {
            tap.push(r);
        }
        let s = tap.save(&path, "l", false).expect("saved");
        assert_eq!(s.sessions, 1);
        assert_eq!(
            tap.save(&path, "l", false).map(|_| ()).map_err(|e| e.code()),
            Err("capture_exists")
        );
        let back = CaptureFile::load(&path).expect("loads");
        assert_eq!(back.sessions(), vec![session(3)]);
        let names: Vec<_> = std::fs::read_dir(&dir).expect("ls").flatten().collect();
        assert_eq!(names.len(), 1, "no tmp file left");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
