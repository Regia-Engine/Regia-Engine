// SPDX-License-Identifier: MPL-2.0
//! Lockstep match replays: the `.lsreplay` v1 file, its writer, its bounded reader, the bit-exact
//! audit and the replay library (spec 120 § 7.6; ADR-8513; requirements from a DO27 design).
//!
//! A lockstep match is its inputs: every participant steps the same frames from the same start.
//! A `.lsreplay` keeps exactly that, plus enough to check a re-run bit for bit:
//!
//! ```text
//! magic b"RGLSRPLY" | version u32 (1) | header_len u32 | header
//! header: game_id str | protocol_version u32 | protocol_id u64 | content_hash u64
//!         | engine_version str | tick_rate u32 | seed u64 | full_every u32 | hash_every u32
//!         | seat count u16 | seat u16 * count
//! record: kind u8 | len u32 | payload (len bytes)
//!   0x01 Full     flags u8 (bit 0: the match start) | tick u64 | world hash u64 | state bytes
//!   0x02 Inputs   tick u64 | delay u16 | delay_from u64 | count u16
//!                 | count * (seat u16 | present u8 | [len u16 | input bytes])
//!                 | membership count u16 | count * (seat u16 | joins u8)
//!   0x03 Hashes   tick u64 | world hash u64 | count u8 | count * (name str8 | hash u64)
//!   0x05 PeerCmds tick u64 | count u16 | count * (seat u16 | ok u8 | kind str8 | text str)
//!   0x07 Abort    tick u64 | code str8 | reason str
//! ```
//!
//! Integers are little endian; `str` is a `u16` length plus UTF-8 and `str8` a `u8` length plus
//! UTF-8. Inputs are the bytes each seat sent ([`crate::NetCodec`]), never re-encoded. A Full is
//! the game's own state encoding after its tick (or before the first frame, for the match start).
//!
//! * [`LsReplayWriter`]: driven by the lockstep host or a seat, opt-in
//!   ([`LsReplaySettings::record`]); a Full every N ticks, the hashes at the checkpoint ticks, and
//!   a byte cap per [`ReplayTier`] that sheds Fulls first and then stops at a whole tick.
//! * [`LsReplayReader`]: every length is checked against the bytes left and the caps before
//!   anything is read or allocated; a newer version is refused by name.
//! * [`audit`]: re-runs the match through the game's [`DesyncReplay`](crate::desync::DesyncReplay) hook from each Full and
//!   compares every recorded hash bit for bit; `regia net replay-audit` prints it.
//! * [`ReplayLibrary`]: a folder of replays, newest first, rotation, import with a SHA-256
//!   duplicate check, export with a sidecar, damaged files listed apart.
//!
//! This is not the replication demo recorder ([`crate::recorder`]): that one records snapshots of
//! an authoritative server; this one records the frames of a lockstep match.

use std::io::Write;

use thiserror::Error;

use super::{LockstepFrame, MembershipKind, SeatId, SeatInput};
use crate::desync::{BundleFrame, BundleInput, BundleMembership, DesyncHash, HexBytes};
use crate::payload::{NetCodec, NetGame, protocol_id};

mod audit;
mod library;
mod sha256;

pub use audit::{ReplayAudit, audit, audit_with};
pub use library::{
    DEFAULT_KEEP_NEWEST, DamagedReplay, ExportedReplay, LSREPLAY_EXTENSION, META_FORMAT, ReplayInfo,
    ReplayLibrary, ReplayList, RotateReport,
};
pub use sha256::sha256_hex;

#[cfg(test)]
mod tests;

/// The project-relative folder replays are kept in unless the game publishes another
/// (`NetAgentState::set_replay_dir`).
pub const DEFAULT_REPLAY_DIR: &str = "saved/replays";

/// The file magic.
pub const LSREPLAY_MAGIC: &[u8; 8] = b"RGLSRPLY";
/// The format version this build writes and reads.
pub const LSREPLAY_VERSION: u32 = 1;

/// Full record: the game's state after a tick.
pub const RECORD_FULL: u8 = 0x01;
/// Inputs record: one agreed frame.
pub const RECORD_INPUTS: u8 = 0x02;
/// Hashes record: the world and per-subsystem hashes at a checkpoint tick.
pub const RECORD_HASHES: u8 = 0x03;
/// Peer commands record: the commands the host applied at a tick (the audit skips them).
pub const RECORD_PEER_CMDS: u8 = 0x05;
/// Abort record: written once at close when the match aborted (a desync, a host fault).
pub const RECORD_ABORT: u8 = 0x07;

/// Largest file the reader takes: the standard tier's cap plus the abort reserve.
pub const MAX_LSREPLAY_BYTES: u64 = (256 << 20) + ABORT_RESERVE_BYTES;
/// Bytes the abort trailer may go past the byte cap (it is always written).
pub const ABORT_RESERVE_BYTES: u64 = 4 << 10;
/// Largest header, bytes.
pub const MAX_HEADER_BYTES: usize = 64 << 10;
/// Largest Full payload (a game state), bytes.
pub const MAX_FULL_BYTES: usize = 32 << 20;
/// Largest payload of any other record, bytes.
pub const MAX_RECORD_BYTES: usize = 4 << 20;
/// Most seats a header or a frame lists.
pub const MAX_SEATS: usize = 1_024;
/// Largest encoded input of one seat, bytes.
pub const MAX_INPUT_BYTES: usize = 60_000;
/// Most subsystems a Hashes record lists ([`crate::desync::MAX_SUBSYSTEMS`]).
pub const MAX_HASHED_SUBSYSTEMS: usize = crate::desync::MAX_SUBSYSTEMS;
/// Most peer commands of one tick.
pub const MAX_PEER_CMDS: usize = 256;
/// Longest game id, bytes.
pub const MAX_GAME_ID_BYTES: usize = 128;
/// Longest `str` field (an engine version, a command text, an abort reason), bytes.
pub const MAX_TEXT_BYTES: usize = 1_024;
/// Longest `str8` field (a subsystem name, a command kind, an abort code), bytes.
pub const MAX_NAME_BYTES: usize = 64;

/// A device tier's replay budget (the byte cap and the Full rate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayTier {
    /// 256 MiB, a Full twice a second.
    #[default]
    Standard,
    /// 64 MiB, a Full once a second (a low-end device).
    Low,
}

impl ReplayTier {
    /// The tier's byte cap.
    pub fn max_bytes(self) -> u64 {
        match self {
            Self::Standard => 256 << 20,
            Self::Low => 64 << 20,
        }
    }

    /// Fulls per second.
    pub fn full_hz(self) -> u32 {
        match self {
            Self::Standard => 2,
            Self::Low => 1,
        }
    }
}

/// Replay recording settings (match settings of the recording participant only).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct LsReplaySettings {
    /// Record at all (opt-in; off by default).
    pub record: bool,
    /// The budget tier.
    pub tier: ReplayTier,
    /// Ticks between Fulls; `None`: the tier's rate at the match's tick rate.
    pub full_every_ticks: Option<u32>,
    /// Ticks between Hashes records (`0`: none); the lockstep checkpoint interval by default.
    pub hash_every_ticks: u32,
    /// The byte cap; `None`: the tier's. At most the standard tier's.
    pub max_bytes: Option<u64>,
    /// Replays the library keeps ([`ReplayLibrary::rotate`]).
    pub keep_newest: usize,
}

impl Default for LsReplaySettings {
    fn default() -> Self {
        Self {
            record: false,
            tier: ReplayTier::Standard,
            full_every_ticks: None,
            hash_every_ticks: super::LockstepConfig::default().hash_every,
            max_bytes: None,
            keep_newest: DEFAULT_KEEP_NEWEST,
        }
    }
}

impl LsReplaySettings {
    /// Ticks between Fulls at `tick_rate` (at least 1).
    pub fn full_every(&self, tick_rate: u32) -> u32 {
        self.full_every_ticks
            .unwrap_or_else(|| tick_rate.max(1) / self.tier.full_hz())
            .max(1)
    }

    /// The byte cap in force.
    pub fn cap(&self) -> u64 {
        self.max_bytes
            .unwrap_or_else(|| self.tier.max_bytes())
            .min(ReplayTier::Standard.max_bytes())
    }
}

/// The file header: which game and build recorded the match, and how.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LsReplayHeader {
    /// `NetGame::GAME_ID`.
    pub game_id: String,
    /// `NetGame::PROTOCOL_VERSION` (the game version).
    pub protocol_version: u32,
    /// [`protocol_id`] of the game (its id, version, content hash and the wire version).
    pub protocol_id: u64,
    /// `NetGame::content_hash`.
    pub content_hash: u64,
    /// The `regia_net` version that wrote the file (information only).
    pub engine_version: String,
    /// Ticks per second.
    pub tick_rate: u32,
    /// The match seed.
    pub seed: u64,
    /// Ticks between Fulls.
    pub full_every_ticks: u32,
    /// Ticks between Hashes records (`0`: none).
    pub hash_every_ticks: u32,
    /// The seats at the start, ascending.
    pub seats: Vec<u16>,
}

impl LsReplayHeader {
    /// The header of a match of game `G` with `seats`, at `tick_rate`, seeded `seed`.
    pub fn of<G: NetGame>(
        seats: impl IntoIterator<Item = SeatId>,
        tick_rate: u32,
        seed: u64,
        settings: &LsReplaySettings,
    ) -> Self {
        let mut seats: Vec<u16> = seats.into_iter().map(|s| s.0).collect();
        seats.sort_unstable();
        seats.dedup();
        Self {
            game_id: G::GAME_ID.to_owned(),
            protocol_version: G::PROTOCOL_VERSION,
            protocol_id: protocol_id::<G>(),
            content_hash: G::content_hash(),
            engine_version: env!("CARGO_PKG_VERSION").to_owned(),
            tick_rate,
            seed,
            full_every_ticks: settings.full_every(tick_rate),
            hash_every_ticks: settings.hash_every_ticks,
            seats,
        }
    }
}

/// A Full record: the game's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullRecord {
    /// The state before the first frame (the match start); `tick` is then that frame's tick.
    pub start: bool,
    /// The tick the state was taken after.
    pub tick: u64,
    /// The world hash of the state ([`DesyncHash::world_hash`]).
    pub world: u64,
    /// The game's state encoding.
    pub state: Vec<u8>,
}

/// An Inputs record: one agreed frame, with the bytes each seat sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputsRecord {
    /// The frame (tick, inputs ascending by seat, membership), as the desync bundle holds it.
    pub frame: BundleFrame,
    /// The input delay in force from `delay_from`.
    pub delay: u16,
    /// The first capture tick `delay` applies to.
    pub delay_from: u64,
}

/// A Hashes record: the hashes at a checkpoint tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashesRecord {
    /// The tick the state was hashed after.
    pub tick: u64,
    /// The world hash.
    pub world: u64,
    /// Every subsystem's hash, in the game's order.
    pub subsystems: Vec<(String, u64)>,
}

/// One peer command the host applied.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PeerCmd {
    /// The seat that sent it.
    pub seat: u16,
    /// Whether it was applied.
    pub ok: bool,
    /// The command (at most [`MAX_NAME_BYTES`]).
    pub kind: String,
    /// Its text or the refusal (at most [`MAX_TEXT_BYTES`]).
    pub text: String,
}

/// A PeerCmds record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerCmdsRecord {
    /// The tick they were applied at.
    pub tick: u64,
    /// The commands, in the order applied.
    pub cmds: Vec<PeerCmd>,
}

/// An Abort record: why and when the match aborted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReplayAbortInfo {
    /// The tick it aborted at.
    pub tick: u64,
    /// The reason code (lowercase ascii, digits and `_`, such as `desync` or `host_desync`).
    pub code: String,
    /// The reason for a person.
    pub reason: String,
}

impl ReplayAbortInfo {
    /// An abort at `tick` with `code` and `reason`.
    pub fn new(tick: u64, code: &str, reason: &str) -> Self {
        Self {
            tick,
            code: code.to_owned(),
            reason: reason.to_owned(),
        }
    }
}

/// One record of a `.lsreplay`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LsRecord {
    /// 0x01.
    Full(FullRecord),
    /// 0x02.
    Inputs(InputsRecord),
    /// 0x03.
    Hashes(HashesRecord),
    /// 0x05.
    PeerCmds(PeerCmdsRecord),
    /// 0x07.
    Abort(ReplayAbortInfo),
}

impl LsRecord {
    /// The record's kind byte.
    pub fn kind(&self) -> u8 {
        match self {
            Self::Full(_) => RECORD_FULL,
            Self::Inputs(_) => RECORD_INPUTS,
            Self::Hashes(_) => RECORD_HASHES,
            Self::PeerCmds(_) => RECORD_PEER_CMDS,
            Self::Abort(_) => RECORD_ABORT,
        }
    }

    /// The tick the record is about.
    pub fn tick(&self) -> u64 {
        match self {
            Self::Full(r) => r.tick,
            Self::Inputs(r) => r.frame.tick,
            Self::Hashes(r) => r.tick,
            Self::PeerCmds(r) => r.tick,
            Self::Abort(r) => r.tick,
        }
    }
}

/// Why a replay could not be read, written, audited or filed. [`LsReplayError::code`] is the
/// agent and CLI error code.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LsReplayError {
    /// The file does not exist.
    #[error("no replay at {0}")]
    NotFound(String),
    /// Reading or writing failed.
    #[error("{0}")]
    Io(String),
    /// Not a `.lsreplay` (the magic or the extension).
    #[error("not a lockstep replay (.lsreplay): {0}")]
    NotLsReplay(String),
    /// A newer format version than this build reads.
    #[error(
        "the replay is format version {found}; this build reads version {LSREPLAY_VERSION}: open it with a newer build of the engine"
    )]
    VersionNewer {
        /// The version in the file.
        found: u32,
    },
    /// The bytes end inside the header or a record.
    #[error("the replay is truncated at byte {at}: {what} needs {need} bytes, {have} are left")]
    Truncated {
        /// Offset of the part that is cut.
        at: u64,
        /// What was being read.
        what: &'static str,
        /// Bytes it needs.
        need: u64,
        /// Bytes left.
        have: u64,
    },
    /// A length is over a cap.
    #[error("{what} is {len} bytes, more than the {max} allowed")]
    TooLarge {
        /// What.
        what: &'static str,
        /// Its length.
        len: u64,
        /// The cap.
        max: u64,
    },
    /// A record kind this version does not know.
    #[error("unknown record kind {kind:#04x} at byte {at}")]
    UnknownRecord {
        /// The kind byte.
        kind: u8,
        /// Its offset.
        at: u64,
    },
    /// A value is out of range or a payload is inconsistent.
    #[error("invalid replay: {0}")]
    Invalid(String),
    /// The replay is of another game or game version than the hook.
    #[error(
        "the replay was recorded by {file}; the replay hook is {hook}. A different game or version steps differently: audit it with the build that recorded it"
    )]
    GameMismatch {
        /// The file's game and version.
        file: String,
        /// The hook's.
        hook: String,
    },
    /// No hook is registered for the replay's game.
    #[error("no DesyncReplay hook is registered for game `{game}` (registered: {registered})")]
    NoHook {
        /// The file's game id.
        game: String,
        /// The registered ids, comma separated (`none` when empty).
        registered: String,
    },
    /// The hook refused to load a Full or step a frame.
    #[error("the replay hook failed at tick {tick}: {reason}")]
    HookFailed {
        /// The tick.
        tick: u64,
        /// The hook's reason.
        reason: String,
    },
    /// The library already holds these bytes.
    #[error("the replay library already holds this replay (same SHA-256) as {existing}")]
    Duplicate {
        /// The file name already in the library.
        existing: String,
    },
}

impl LsReplayError {
    /// The error code (lowercase ascii, digits and `_`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "replay_not_found",
            Self::Io(_) => "replay_io",
            Self::NotLsReplay(_) => "replay_not_lsreplay",
            Self::VersionNewer { .. } => "replay_version_newer",
            Self::Truncated { .. } => "replay_truncated",
            Self::TooLarge { .. } => "replay_too_large",
            Self::UnknownRecord { .. } => "replay_unknown_record",
            Self::Invalid(_) => "replay_invalid",
            Self::GameMismatch { .. } => "replay_game_mismatch",
            Self::NoHook { .. } => "replay_no_hook",
            Self::HookFailed { .. } => "replay_hook_failed",
            Self::Duplicate { .. } => "replay_duplicate",
        }
    }

    /// Every code [`Self::code`] returns (the book page lists them).
    pub const CODES: [&'static str; 12] = [
        "replay_not_found",
        "replay_io",
        "replay_not_lsreplay",
        "replay_version_newer",
        "replay_truncated",
        "replay_too_large",
        "replay_unknown_record",
        "replay_invalid",
        "replay_game_mismatch",
        "replay_no_hook",
        "replay_hook_failed",
        "replay_duplicate",
    ];

    fn io(err: &std::io::Error) -> Self {
        Self::Io(err.to_string())
    }
}

// ---------------------------------------------------------------------------------------------
// The byte codec (private: the smallest one this format needs; ADR-8513 says why it is not the
// detsim state codec).

struct Put(Vec<u8>);

impl Put {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn str8(&mut self, s: &str, what: &str) -> Result<(), LsReplayError> {
        if s.len() > MAX_NAME_BYTES {
            return Err(LsReplayError::Invalid(format!(
                "{what} `{s}` is {} bytes, more than {MAX_NAME_BYTES}",
                s.len()
            )));
        }
        self.u8(s.len() as u8);
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
    fn str16(&mut self, s: &str, max: usize, what: &str) -> Result<(), LsReplayError> {
        if s.len() > max {
            return Err(LsReplayError::Invalid(format!(
                "{what} is {} bytes, more than {max}",
                s.len()
            )));
        }
        self.u16(s.len() as u16);
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// A bounded cursor over a borrowed slice: every read checks the bytes left first, and nothing
/// is allocated larger than the bytes that exist.
struct Take<'a> {
    buf: &'a [u8],
    pos: usize,
    /// Offset of `buf[0]` in the file, for messages.
    base: u64,
    /// What a short read is reported as: the file ends (`Truncated`) or a payload is shorter than
    /// its fields (`Invalid`).
    in_payload: Option<u8>,
}

impl<'a> Take<'a> {
    fn new(buf: &'a [u8], base: u64, in_payload: Option<u8>) -> Self {
        Self {
            buf,
            pos: 0,
            base,
            in_payload,
        }
    }

    fn left(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn bytes(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], LsReplayError> {
        if n > self.left() {
            let at = self.base + self.pos as u64;
            return Err(match self.in_payload {
                Some(kind) => LsReplayError::Invalid(format!(
                    "record {kind:#04x} at byte {}: {what} needs {n} bytes, the payload has {} left",
                    self.base,
                    self.left()
                )),
                None => LsReplayError::Truncated {
                    at,
                    what,
                    need: n as u64,
                    have: self.left() as u64,
                },
            });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    fn array<const N: usize>(&mut self, what: &'static str) -> Result<[u8; N], LsReplayError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.bytes(N, what)?);
        Ok(out)
    }
    fn u8(&mut self, what: &'static str) -> Result<u8, LsReplayError> {
        Ok(self.array::<1>(what)?[0])
    }
    fn u16(&mut self, what: &'static str) -> Result<u16, LsReplayError> {
        Ok(u16::from_le_bytes(self.array(what)?))
    }
    fn u32(&mut self, what: &'static str) -> Result<u32, LsReplayError> {
        Ok(u32::from_le_bytes(self.array(what)?))
    }
    fn u64(&mut self, what: &'static str) -> Result<u64, LsReplayError> {
        Ok(u64::from_le_bytes(self.array(what)?))
    }
    fn bool(&mut self, what: &'static str) -> Result<bool, LsReplayError> {
        match self.u8(what)? {
            0 => Ok(false),
            1 => Ok(true),
            v => Err(LsReplayError::Invalid(format!("{what} is {v}, not 0 or 1"))),
        }
    }
    fn utf8(&mut self, len: usize, max: usize, what: &'static str) -> Result<String, LsReplayError> {
        if len > max {
            return Err(LsReplayError::TooLarge {
                what,
                len: len as u64,
                max: max as u64,
            });
        }
        let raw = self.bytes(len, what)?;
        std::str::from_utf8(raw)
            .map(str::to_owned)
            .map_err(|_| LsReplayError::Invalid(format!("{what} is not UTF-8")))
    }
    fn str8(&mut self, what: &'static str) -> Result<String, LsReplayError> {
        let len = usize::from(self.u8(what)?);
        self.utf8(len, MAX_NAME_BYTES, what)
    }
    fn str16(&mut self, max: usize, what: &'static str) -> Result<String, LsReplayError> {
        let len = usize::from(self.u16(what)?);
        self.utf8(len, max, what)
    }
    /// A count of items at least `min_item` bytes each: refused above `max`, and never more
    /// than the bytes left could hold (so a `Vec::with_capacity` of it is bounded by real bytes).
    fn count(
        &mut self,
        n: usize,
        max: usize,
        min_item: usize,
        what: &'static str,
    ) -> Result<usize, LsReplayError> {
        if n > max {
            return Err(LsReplayError::TooLarge {
                what,
                len: n as u64,
                max: max as u64,
            });
        }
        if n.saturating_mul(min_item) > self.left() {
            // Reported through the same path as any short read.
            self.bytes(n.saturating_mul(min_item), what)?;
        }
        Ok(n)
    }
    fn finish(&self, what: &'static str) -> Result<(), LsReplayError> {
        if self.left() == 0 {
            Ok(())
        } else {
            Err(LsReplayError::Invalid(format!(
                "{} trailing bytes after the {what} at byte {}",
                self.left(),
                self.base
            )))
        }
    }
}

fn encode_header(h: &LsReplayHeader) -> Result<Vec<u8>, LsReplayError> {
    if h.game_id.is_empty() {
        return Err(LsReplayError::Invalid("the game id is empty".into()));
    }
    if h.seats.len() > MAX_SEATS || h.seats.windows(2).any(|w| w[0] >= w[1]) {
        return Err(LsReplayError::Invalid(format!(
            "the header lists {} seats; at most {MAX_SEATS}, ascending, each once",
            h.seats.len()
        )));
    }
    let mut p = Put(Vec::with_capacity(64 + h.game_id.len() + 2 * h.seats.len()));
    p.str16(&h.game_id, MAX_GAME_ID_BYTES, "the game id")?;
    p.u32(h.protocol_version);
    p.u64(h.protocol_id);
    p.u64(h.content_hash);
    p.str16(&h.engine_version, MAX_TEXT_BYTES, "the engine version")?;
    p.u32(h.tick_rate);
    p.u64(h.seed);
    p.u32(h.full_every_ticks);
    p.u32(h.hash_every_ticks);
    p.u16(h.seats.len() as u16);
    for seat in &h.seats {
        p.u16(*seat);
    }
    Ok(p.0)
}

fn decode_header(t: &mut Take<'_>) -> Result<LsReplayHeader, LsReplayError> {
    let game_id = t.str16(MAX_GAME_ID_BYTES, "the game id")?;
    if game_id.is_empty() {
        return Err(LsReplayError::Invalid("the game id is empty".into()));
    }
    let protocol_version = t.u32("the protocol version")?;
    let protocol_id = t.u64("the protocol id")?;
    let content_hash = t.u64("the content hash")?;
    let engine_version = t.str16(MAX_TEXT_BYTES, "the engine version")?;
    let tick_rate = t.u32("the tick rate")?;
    let seed = t.u64("the seed")?;
    let full_every_ticks = t.u32("the full interval")?;
    let hash_every_ticks = t.u32("the hash interval")?;
    let n = usize::from(t.u16("the seat count")?);
    let n = t.count(n, MAX_SEATS, 2, "the seat list")?;
    let mut seats = Vec::with_capacity(n);
    for _ in 0..n {
        let seat = t.u16("a seat")?;
        if seats.last().is_some_and(|last| *last >= seat) {
            return Err(LsReplayError::Invalid(
                "the header's seats are not ascending".into(),
            ));
        }
        seats.push(seat);
    }
    t.finish("header")?;
    Ok(LsReplayHeader {
        game_id,
        protocol_version,
        protocol_id,
        content_hash,
        engine_version,
        tick_rate,
        seed,
        full_every_ticks,
        hash_every_ticks,
        seats,
    })
}

fn encode_record(record: &LsRecord) -> Result<Vec<u8>, LsReplayError> {
    let mut p = Put(Vec::new());
    match record {
        LsRecord::Full(f) => {
            if f.state.len() > MAX_FULL_BYTES {
                return Err(LsReplayError::TooLarge {
                    what: "a full state",
                    len: f.state.len() as u64,
                    max: MAX_FULL_BYTES as u64,
                });
            }
            p.0.reserve(17 + f.state.len());
            p.u8(u8::from(f.start));
            p.u64(f.tick);
            p.u64(f.world);
            p.0.extend_from_slice(&f.state);
        }
        LsRecord::Inputs(r) => {
            let f = &r.frame;
            if f.inputs.len() > MAX_SEATS || f.membership.len() > MAX_SEATS {
                return Err(LsReplayError::Invalid(format!(
                    "tick {}: more than {MAX_SEATS} seats",
                    f.tick
                )));
            }
            p.u64(f.tick);
            p.u16(r.delay);
            p.u64(r.delay_from);
            p.u16(f.inputs.len() as u16);
            for input in &f.inputs {
                p.u16(input.seat);
                match &input.input {
                    Some(bytes) => {
                        if bytes.0.len() > MAX_INPUT_BYTES {
                            return Err(LsReplayError::TooLarge {
                                what: "an input",
                                len: bytes.0.len() as u64,
                                max: MAX_INPUT_BYTES as u64,
                            });
                        }
                        p.u8(1);
                        p.u16(bytes.0.len() as u16);
                        p.0.extend_from_slice(&bytes.0);
                    }
                    None => p.u8(0),
                }
            }
            p.u16(f.membership.len() as u16);
            for m in &f.membership {
                p.u16(m.seat);
                p.u8(u8::from(m.joins));
            }
        }
        LsRecord::Hashes(h) => {
            if h.subsystems.len() > MAX_HASHED_SUBSYSTEMS {
                return Err(LsReplayError::Invalid(format!(
                    "tick {}: more than {MAX_HASHED_SUBSYSTEMS} subsystems",
                    h.tick
                )));
            }
            p.u64(h.tick);
            p.u64(h.world);
            p.u8(h.subsystems.len() as u8);
            for (name, hash) in &h.subsystems {
                p.str8(name, "a subsystem name")?;
                p.u64(*hash);
            }
        }
        LsRecord::PeerCmds(c) => {
            if c.cmds.len() > MAX_PEER_CMDS {
                return Err(LsReplayError::Invalid(format!(
                    "tick {}: more than {MAX_PEER_CMDS} peer commands",
                    c.tick
                )));
            }
            p.u64(c.tick);
            p.u16(c.cmds.len() as u16);
            for cmd in &c.cmds {
                p.u16(cmd.seat);
                p.u8(u8::from(cmd.ok));
                p.str8(&cmd.kind, "a command kind")?;
                p.str16(&cmd.text, MAX_TEXT_BYTES, "a command text")?;
            }
        }
        LsRecord::Abort(a) => {
            p.u64(a.tick);
            p.str8(&a.code, "the abort code")?;
            p.str16(&a.reason, MAX_TEXT_BYTES, "the abort reason")?;
        }
    }
    let max = if record.kind() == RECORD_FULL {
        MAX_FULL_BYTES + 17
    } else {
        MAX_RECORD_BYTES
    };
    if p.0.len() > max {
        return Err(LsReplayError::TooLarge {
            what: "a record",
            len: p.0.len() as u64,
            max: max as u64,
        });
    }
    let mut out = Vec::with_capacity(5 + p.0.len());
    out.push(record.kind());
    out.extend_from_slice(&(p.0.len() as u32).to_le_bytes());
    out.extend_from_slice(&p.0);
    Ok(out)
}

fn decode_payload(kind: u8, payload: &[u8], at: u64) -> Result<LsRecord, LsReplayError> {
    let mut t = Take::new(payload, at, Some(kind));
    let record = match kind {
        RECORD_FULL => {
            let flags = t.u8("the full flags")?;
            if flags > 1 {
                return Err(LsReplayError::Invalid(format!(
                    "full at byte {at}: flags {flags:#04x}"
                )));
            }
            let tick = t.u64("the tick")?;
            let world = t.u64("the world hash")?;
            let state = t.bytes(t.left(), "the state")?.to_vec();
            LsRecord::Full(FullRecord {
                start: flags == 1,
                tick,
                world,
                state,
            })
        }
        RECORD_INPUTS => {
            let tick = t.u64("the tick")?;
            let delay = t.u16("the delay")?;
            let delay_from = t.u64("the delay start")?;
            let n = usize::from(t.u16("the seat count")?);
            let n = t.count(n, MAX_SEATS, 3, "the inputs")?;
            let mut inputs = Vec::with_capacity(n);
            for _ in 0..n {
                let seat = t.u16("a seat")?;
                if inputs.last().is_some_and(|last: &BundleInput| last.seat >= seat) {
                    return Err(LsReplayError::Invalid(format!(
                        "tick {tick}: the seats are not ascending"
                    )));
                }
                let input = if t.bool("an input flag")? {
                    let len = usize::from(t.u16("an input length")?);
                    if len > MAX_INPUT_BYTES {
                        return Err(LsReplayError::TooLarge {
                            what: "an input",
                            len: len as u64,
                            max: MAX_INPUT_BYTES as u64,
                        });
                    }
                    Some(HexBytes(t.bytes(len, "an input")?.to_vec()))
                } else {
                    None
                };
                inputs.push(BundleInput { seat, input });
            }
            let m = usize::from(t.u16("the membership count")?);
            let m = t.count(m, MAX_SEATS, 3, "the membership")?;
            let mut membership = Vec::with_capacity(m);
            for _ in 0..m {
                let seat = t.u16("a seat")?;
                let joins = t.bool("a membership kind")?;
                membership.push(BundleMembership { seat, joins });
            }
            LsRecord::Inputs(InputsRecord {
                frame: BundleFrame {
                    tick,
                    inputs,
                    membership,
                },
                delay,
                delay_from,
            })
        }
        RECORD_HASHES => {
            let tick = t.u64("the tick")?;
            let world = t.u64("the world hash")?;
            let n = usize::from(t.u8("the subsystem count")?);
            let n = t.count(n, MAX_HASHED_SUBSYSTEMS, 9, "the subsystems")?;
            let mut subsystems = Vec::with_capacity(n);
            for _ in 0..n {
                let name = t.str8("a subsystem name")?;
                subsystems.push((name, t.u64("a subsystem hash")?));
            }
            LsRecord::Hashes(HashesRecord {
                tick,
                world,
                subsystems,
            })
        }
        RECORD_PEER_CMDS => {
            let tick = t.u64("the tick")?;
            let n = usize::from(t.u16("the command count")?);
            let n = t.count(n, MAX_PEER_CMDS, 6, "the commands")?;
            let mut cmds = Vec::with_capacity(n);
            for _ in 0..n {
                let seat = t.u16("a seat")?;
                let ok = t.bool("a command result")?;
                let kind = t.str8("a command kind")?;
                let text = t.str16(MAX_TEXT_BYTES, "a command text")?;
                cmds.push(PeerCmd { seat, ok, kind, text });
            }
            LsRecord::PeerCmds(PeerCmdsRecord { tick, cmds })
        }
        RECORD_ABORT => {
            let tick = t.u64("the tick")?;
            let code = t.str8("the abort code")?;
            let reason = t.str16(MAX_TEXT_BYTES, "the abort reason")?;
            LsRecord::Abort(ReplayAbortInfo { tick, code, reason })
        }
        other => return Err(LsReplayError::UnknownRecord { kind: other, at }),
    };
    t.finish("record")?;
    Ok(record)
}

// ---------------------------------------------------------------------------------------------
// Reader.

/// A bounded `.lsreplay` reader over bytes in memory.
#[derive(Debug, Clone)]
pub struct LsReplayReader<'a> {
    header: LsReplayHeader,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> LsReplayReader<'a> {
    /// Reads the magic, the version and the header. A newer version is refused before its header
    /// is read.
    pub fn new(bytes: &'a [u8]) -> Result<Self, LsReplayError> {
        if bytes.len() as u64 > MAX_LSREPLAY_BYTES {
            return Err(LsReplayError::TooLarge {
                what: "the replay",
                len: bytes.len() as u64,
                max: MAX_LSREPLAY_BYTES,
            });
        }
        let (header, pos) = read_head(bytes)?;
        Ok(Self { header, bytes, pos })
    }

    /// The header.
    pub fn header(&self) -> &LsReplayHeader {
        &self.header
    }

    /// The offset of the next record.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// The next record (`None` at the end). Its length is checked against the bytes left and the
    /// record caps before its payload is touched.
    pub fn next_record(&mut self) -> Result<Option<LsRecord>, LsReplayError> {
        let Some((kind, start, len)) = next_frame(self.bytes, self.pos)? else {
            return Ok(None);
        };
        let record = decode_payload(kind, &self.bytes[start..start + len], self.pos as u64)?;
        self.pos = start + len;
        Ok(Some(record))
    }

    /// Reads every record to the end (a whole-file validation).
    pub fn read_all(mut self) -> Result<Vec<LsRecord>, LsReplayError> {
        let mut out = Vec::new();
        while let Some(record) = self.next_record()? {
            out.push(record);
        }
        Ok(out)
    }
}

/// The magic, the version and the header of `bytes`: the header and the offset after it.
fn read_head(bytes: &[u8]) -> Result<(LsReplayHeader, usize), LsReplayError> {
    let mut t = Take::new(bytes, 0, None);
    let magic = t.bytes(LSREPLAY_MAGIC.len(), "the magic").map_err(|e| match e {
        LsReplayError::Truncated { .. } if !LSREPLAY_MAGIC.starts_with(bytes) => {
            LsReplayError::NotLsReplay("the file does not start with the magic".into())
        }
        other => other,
    })?;
    if magic != LSREPLAY_MAGIC {
        return Err(LsReplayError::NotLsReplay(
            "the file does not start with the magic".into(),
        ));
    }
    let version = t.u32("the version")?;
    if version > LSREPLAY_VERSION {
        return Err(LsReplayError::VersionNewer { found: version });
    }
    if version == 0 {
        return Err(LsReplayError::Invalid("format version 0".into()));
    }
    let len = t.u32("the header length")? as usize;
    if len > MAX_HEADER_BYTES {
        return Err(LsReplayError::TooLarge {
            what: "the header",
            len: len as u64,
            max: MAX_HEADER_BYTES as u64,
        });
    }
    let at = t.pos as u64;
    let raw = t.bytes(len, "the header")?;
    let header = decode_header(&mut Take::new(raw, at, None)).map_err(|e| match e {
        // Inside a header whose length is whole, a short field is an inconsistent header.
        LsReplayError::Truncated { what, .. } => {
            LsReplayError::Invalid(format!("the header ends inside {what}"))
        }
        other => other,
    })?;
    Ok((header, t.pos))
}

/// The record frame at `pos`: kind, payload start and payload length, all checked.
fn next_frame(bytes: &[u8], pos: usize) -> Result<Option<(u8, usize, usize)>, LsReplayError> {
    if pos >= bytes.len() {
        return Ok(None);
    }
    let mut t = Take::new(&bytes[pos..], pos as u64, None);
    let kind = t.u8("a record kind")?;
    let len = t.u32("a record length")? as usize;
    let max = if kind == RECORD_FULL {
        MAX_FULL_BYTES + 17
    } else {
        MAX_RECORD_BYTES
    };
    if len > max {
        return Err(LsReplayError::TooLarge {
            what: "a record",
            len: len as u64,
            max: max as u64,
        });
    }
    t.bytes(len, "a record payload")?;
    Ok(Some((kind, pos + 5, len)))
}

// ---------------------------------------------------------------------------------------------
// Writer.

/// What a writer has written and shed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct LsReplayStats {
    /// Bytes written.
    pub bytes: u64,
    /// Records written.
    pub records: u64,
    /// Fulls skipped to stay under the cap.
    pub fulls_shed: u64,
    /// Other records not written once the cap was reached.
    pub records_dropped: u64,
    /// The cap was reached: the file ends at the last whole tick before it.
    pub capped: bool,
}

/// Writes a `.lsreplay` as a lockstep host or seat steps (module docs).
///
/// Call [`Self::record_start`] once before the first frame, then per tick
/// [`Self::record_frame`] with the frame and [`Self::record_state`] after stepping it, and
/// [`Self::finish`] at the end.
#[derive(Debug)]
pub struct LsReplayWriter<W: Write> {
    out: W,
    settings: LsReplaySettings,
    full_every: u64,
    hash_every: u64,
    cap: u64,
    stats: LsReplayStats,
    scratch: Vec<u8>,
    rows: Vec<(&'static str, u64)>,
}

impl<W: Write> LsReplayWriter<W> {
    /// Writes the magic, the version and `header` to `out`. The header's Full and hash intervals
    /// are taken from `settings` when they are set there.
    pub fn new(
        mut out: W,
        mut header: LsReplayHeader,
        settings: LsReplaySettings,
    ) -> Result<Self, LsReplayError> {
        if settings.full_every_ticks.is_some() || header.full_every_ticks == 0 {
            header.full_every_ticks = settings.full_every(header.tick_rate);
        }
        let body = encode_header(&header)?;
        let mut head = Vec::with_capacity(16 + body.len());
        head.extend_from_slice(LSREPLAY_MAGIC);
        head.extend_from_slice(&LSREPLAY_VERSION.to_le_bytes());
        head.extend_from_slice(&(body.len() as u32).to_le_bytes());
        head.extend_from_slice(&body);
        out.write_all(&head).map_err(|e| LsReplayError::io(&e))?;
        Ok(Self {
            out,
            full_every: u64::from(header.full_every_ticks.max(1)),
            hash_every: u64::from(header.hash_every_ticks),
            cap: settings.cap(),
            settings,
            stats: LsReplayStats {
                bytes: head.len() as u64,
                ..LsReplayStats::default()
            },
            scratch: Vec::new(),
            rows: Vec::new(),
        })
    }

    /// The settings the writer was made with.
    pub fn settings(&self) -> &LsReplaySettings {
        &self.settings
    }

    /// What was written so far.
    pub fn stats(&self) -> LsReplayStats {
        self.stats
    }

    /// Writes one record as it is (no cadence, no cap): a tool that re-writes or trims a file.
    pub fn write_record(&mut self, record: &LsRecord) -> Result<(), LsReplayError> {
        let bytes = encode_record(record)?;
        self.out.write_all(&bytes).map_err(|e| LsReplayError::io(&e))?;
        self.stats.bytes += bytes.len() as u64;
        self.stats.records += 1;
        Ok(())
    }

    /// Writes `record` under the cap: a Full that does not fit is shed; anything else that does
    /// not fit stops the file (every later record but the abort is dropped).
    fn capped(&mut self, record: &LsRecord) -> Result<(), LsReplayError> {
        if self.stats.capped {
            self.stats.records_dropped += 1;
            return Ok(());
        }
        let bytes = encode_record(record)?;
        if self.stats.bytes + bytes.len() as u64 > self.cap {
            if record.kind() == RECORD_FULL {
                self.stats.fulls_shed += 1;
            } else {
                self.stats.capped = true;
                self.stats.records_dropped += 1;
                tracing::warn!(
                    bytes = self.stats.bytes,
                    cap = self.cap,
                    tick = record.tick(),
                    "lsreplay: the byte cap is reached; the replay ends here"
                );
            }
            return Ok(());
        }
        self.out.write_all(&bytes).map_err(|e| LsReplayError::io(&e))?;
        self.stats.bytes += bytes.len() as u64;
        self.stats.records += 1;
        Ok(())
    }

    /// The state before the first frame (tick `first_tick` is the first frame's tick, 0 for a
    /// match start): a Full with the start flag. `save` appends the game's state encoding.
    pub fn record_start<H: DesyncHash + ?Sized>(
        &mut self,
        state: &H,
        save: impl FnOnce(&mut Vec<u8>),
    ) -> Result<(), LsReplayError> {
        self.record_start_at(0, state, save)
    }

    /// [`Self::record_start`] for a recording that starts at `first_tick` (a seat that joined late
    /// records from the state it was sent).
    pub fn record_start_at<H: DesyncHash + ?Sized>(
        &mut self,
        first_tick: u64,
        state: &H,
        save: impl FnOnce(&mut Vec<u8>),
    ) -> Result<(), LsReplayError> {
        let world = state.world_hash(first_tick);
        let full = self.full(true, first_tick, world, save);
        self.capped(&full)
    }

    fn full(&mut self, start: bool, tick: u64, world: u64, save: impl FnOnce(&mut Vec<u8>)) -> LsRecord {
        self.scratch.clear();
        save(&mut self.scratch);
        LsRecord::Full(FullRecord {
            start,
            tick,
            world,
            state: std::mem::take(&mut self.scratch),
        })
    }

    /// One agreed frame: its inputs as the bytes the game's codec gives.
    pub fn record_frame<I: NetCodec>(&mut self, frame: &LockstepFrame<I>) -> Result<(), LsReplayError> {
        let _span = tracing::trace_span!("regia_net::lsreplay_frame", tick = frame.tick).entered();
        let inputs = frame
            .inputs
            .iter()
            .map(|(seat, input)| {
                // A held input (the missing-input policy's, ADR-8892 on landing) is recorded as the
                // value the match stepped with, as the desync bundle does: the replay steps the same.
                let input = match input {
                    SeatInput::Present(value) | SeatInput::Held(value) => {
                        Some(HexBytes(value.to_bytes().map_err(|e| {
                            LsReplayError::Invalid(format!(
                                "tick {} {seat}: the input does not encode: {e}",
                                frame.tick
                            ))
                        })?))
                    }
                    SeatInput::Absent => None,
                };
                Ok(BundleInput { seat: seat.0, input })
            })
            .collect::<Result<Vec<_>, LsReplayError>>()?;
        let membership = frame
            .membership
            .iter()
            .map(|m| BundleMembership {
                seat: m.seat.0,
                joins: m.kind == MembershipKind::Join,
            })
            .collect();
        self.capped(&LsRecord::Inputs(InputsRecord {
            frame: BundleFrame {
                tick: frame.tick,
                inputs,
                membership,
            },
            delay: frame.delay,
            delay_from: frame.delay_from,
        }))
    }

    /// The state after stepping `tick`: a Full when `tick + 1` is a multiple of the Full interval
    /// (`save` is called only then), and the hashes when `tick` is a checkpoint (a multiple of the
    /// hash interval).
    pub fn record_state<H: DesyncHash + ?Sized>(
        &mut self,
        tick: u64,
        state: &H,
        save: impl FnOnce(&mut Vec<u8>),
    ) -> Result<(), LsReplayError> {
        let full_due = (tick + 1).is_multiple_of(self.full_every);
        let hash_due = self.hash_every > 0 && tick.is_multiple_of(self.hash_every);
        if !full_due && !hash_due {
            return Ok(());
        }
        let world = state.world_hash(tick);
        if full_due {
            let full = self.full(false, tick, world, save);
            self.capped(&full)?;
            if let LsRecord::Full(f) = full {
                // Keep the buffer for the next Full.
                self.scratch = f.state;
            }
        }
        if hash_due {
            self.rows.clear();
            state.subsystem_hashes(tick, &mut self.rows);
            self.rows.truncate(MAX_HASHED_SUBSYSTEMS);
            let subsystems = self
                .rows
                .iter()
                .map(|(name, hash)| (crate::desync::clip_name(name).to_owned(), *hash))
                .collect();
            self.capped(&LsRecord::Hashes(HashesRecord {
                tick,
                world,
                subsystems,
            }))?;
        }
        Ok(())
    }

    /// The commands the host applied at `tick` (nothing is written for none).
    pub fn record_peer_cmds(&mut self, tick: u64, cmds: &[PeerCmd]) -> Result<(), LsReplayError> {
        if cmds.is_empty() {
            return Ok(());
        }
        self.capped(&LsRecord::PeerCmds(PeerCmdsRecord {
            tick,
            cmds: cmds.to_vec(),
        }))
    }

    /// Ends the file: writes the abort record when the match aborted (always, past the cap by at
    /// most [`ABORT_RESERVE_BYTES`]), flushes, and returns the output.
    pub fn finish(mut self, abort: Option<ReplayAbortInfo>) -> Result<W, LsReplayError> {
        if let Some(abort) = abort {
            self.write_record(&LsRecord::Abort(abort))?;
        }
        self.out.flush().map_err(|e| LsReplayError::io(&e))?;
        Ok(self.out)
    }

    /// The output as it is (no abort record, no flush).
    pub fn into_inner(self) -> W {
        self.out
    }
}

/// Reads a `.lsreplay` file whole, refusing one over [`MAX_LSREPLAY_BYTES`] before reading it
/// (`replay_not_found` when it does not exist).
pub fn read_file(path: &std::path::Path) -> Result<Vec<u8>, LsReplayError> {
    library::read_bounded(path)
}

/// Audits the file at `path` through the hook registered for its game in `registry`.
pub fn audit_file(
    path: &std::path::Path,
    registry: &crate::desync::ReplayRegistry,
) -> Result<ReplayAudit, LsReplayError> {
    audit_with(&read_file(path)?, registry)
}

/// A `.lsreplay` of the engine's sample game ([`crate::desync::sample`], the built-in replay
/// hook): `seats` seats step `ticks` ticks from the match start, a Full every `full_every`
/// ticks and the hashes every `hash_every`. `flip` `(tick, seat)` flips one bit of that seat's
/// input in the file after recording (the audit then names that tick). Deterministic: the CLI and
/// agent tests, the format corpus and the fuzz target use it.
pub fn sample_replay(
    seats: u16,
    ticks: u64,
    full_every: u32,
    hash_every: u32,
    flip: Option<(u64, u16)>,
) -> Vec<u8> {
    use crate::desync::sample::{SampleGame, SampleSim, sample_input};
    let settings = LsReplaySettings {
        record: true,
        full_every_ticks: Some(full_every),
        hash_every_ticks: hash_every,
        ..LsReplaySettings::default()
    };
    let seats = seats.clamp(1, MAX_SEATS as u16);
    let header = LsReplayHeader::of::<SampleGame>((0..seats).map(SeatId), 60, 0x5eed, &settings);
    let Ok(mut writer) = LsReplayWriter::new(Vec::new(), header, settings) else {
        return Vec::new();
    };
    let mut sim = SampleSim::default();
    let mut ok = writer.record_start(&sim, |o| o.extend(sim.snapshot())).is_ok();
    for tick in 0..ticks {
        let inputs: Vec<(SeatId, SeatInput<u8>)> = (0..seats)
            .map(|s| {
                let input = sample_input(s, tick) ^ u8::from(flip == Some((tick, s)));
                (SeatId(s), SeatInput::Present(input))
            })
            .collect();
        let frame = LockstepFrame {
            tick,
            inputs,
            delay: 4,
            delay_from: 0,
            membership: Vec::new(),
            match_end: false,
        };
        // The file gets the flipped input (a bit that went bad on disk); the match stepped the
        // true one.
        let mut stepped = frame.clone();
        if let Some((t, s)) = flip
            && t == tick
            && let Some((_, SeatInput::Present(v))) = stepped.inputs.get_mut(usize::from(s))
        {
            *v ^= 1;
        }
        ok &= writer.record_frame(&frame).is_ok();
        sim.step_inputs(
            tick,
            stepped.inputs.iter().map(|(s, i)| (s.0, i.as_ref().copied())),
        );
        ok &= writer
            .record_state(tick, &sim, |o| o.extend(sim.snapshot()))
            .is_ok();
    }
    if ok {
        writer.finish(None).unwrap_or_default()
    } else {
        writer.into_inner()
    }
}

/// The `.lsreplay` layout for `cargo xtask check-formats` (ADR-8513): a change to any field here is
/// a format change (a version bump and a migrator).
pub fn lsreplay_schema_fingerprint() -> String {
    concat!(
        "magic:RGLSRPLY;version:u32;header_len:u32;",
        "header:game_id(str16),protocol_version(u32),protocol_id(u64),content_hash(u64),engine_version(str16),",
        "tick_rate(u32),seed(u64),full_every(u32),hash_every(u32),seats(u16,u16*);",
        "record:kind(u8),len(u32),payload;",
        "1=full[flags(u8),tick(u64),world(u64),state];",
        "2=inputs[tick(u64),delay(u16),delay_from(u64),inputs(u16,(seat(u16),present(u8),[len(u16),bytes])*),",
        "membership(u16,(seat(u16),joins(u8))*)];",
        "3=hashes[tick(u64),world(u64),subsystems(u8,(name(str8),hash(u64))*)];",
        "5=peer_cmds[tick(u64),cmds(u16,(seat(u16),ok(u8),kind(str8),text(str16))*)];",
        "7=abort[tick(u64),code(str8),reason(str16)]",
    )
    .to_owned()
}

/// The `from` versions of the `.lsreplay` migrations: none, the format has one version.
pub fn lsreplay_migration_sources() -> Vec<u32> {
    (1..LSREPLAY_VERSION).collect()
}

/// The format version of a `.lsreplay`'s bytes, for `regia upgrade-project`: `Ok(None)` when they
/// are not one.
pub fn lsreplay_file_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    if bytes.get(..LSREPLAY_MAGIC.len()) != Some(&LSREPLAY_MAGIC[..]) {
        return Ok(None);
    }
    let Some(word) = bytes.get(LSREPLAY_MAGIC.len()..LSREPLAY_MAGIC.len() + 4) else {
        return Err("the header is cut".to_owned());
    };
    Ok(Some(u32::from_le_bytes([word[0], word[1], word[2], word[3]])))
}

/// Decodes every record of `bytes` and writes them again (the fuzz target's and the format
/// corpus's round trip): the same bytes for a file this build wrote.
pub fn read_whole(bytes: &[u8]) -> Result<Vec<u8>, LsReplayError> {
    let reader = LsReplayReader::new(bytes)?;
    let header = reader.header().clone();
    let records = reader.read_all()?;
    let mut writer = LsReplayWriter::new(
        Vec::with_capacity(bytes.len()),
        header,
        LsReplaySettings::default(),
    )?;
    for record in &records {
        writer.write_record(record)?;
    }
    Ok(writer.into_inner())
}
