// SPDX-License-Identifier: MPL-2.0
//! `ReplaySim`: an [`ExternalSim`] that plays a Direct Order 27 `.do27replay` (version 2, 3 or 4) file
//! deterministically, for rendering, camera and VFX work (`docs/specs/25-sim-bridge-and-do27.md`).
//!
//! # What a replay holds, and what this reads
//!
//! The byte layout is the owner's public note `spec/replay/do27-replay-v3.md` (header, player and
//! peer tables, then records: `0x01` full, `0x02` applied inputs, `0x03` salvage ledger, `0x04`
//! nav, `0x05` peer commands, `0x06` Moments, `0x07` abort). [`ReplayFile::parse`] walks every
//! record from those lengths alone and never panics: malformed input is a [`ReplayError`].
//! Version 4 adds player/team labels and bounded map/training records (`docs/formats/do27replay.md`).
//! These are validated and skipped without changing playback. Other unrecognized record tags
//! remain errors: the format has no universal record-length envelope (ADR-8191).
//!
//! The snapshot inside a *full* is **opaque** to an outside reader: its layout changes with each
//! sim build and the `state_hash` is a function of that layout. So this module does not decode
//! it. A game that links the matching sim plugs in a [`FullCodec`], which turns the bytes into
//! render-ready [`ReplayEntity`] values and (optionally) recomputes the state hash;
//! [`ReplayFile::verify`] then compares recomputed against stored hashes bit for bit. Without a
//! codec ([`OpaqueCodec`]) the hash check is *skipped, not passed*: the report says how many
//! fulls were checked. Everything else a replay carries (nav beacons, applied inputs, peer
//! commands, Moments) has a fixed layout and is read directly.
//!
//! # Playback
//!
//! Playback never re-simulates: between two fulls the entity set is the last full's, and the nav
//! beacons are the last nav record's. Time is integer: the clock accumulates
//! `frame_time × tick_rate × speed` in exact integer units, so the same frame times always land on
//! the same ticks. [`ReplaySim::seek`] and [`ReplaySim::step`] are tick-exact and independent of
//! any frame clock; events (Moments, peer commands, the abort flag) fire exactly once, when
//! playback crosses their tick forward. A seek fires nothing.
//!
//! Sub-tick blending (opt-in, [`ReplaySim::set_interpolation`], ADR-1519): slow-motion playback
//! sits between ticks for several frames. With blending on, the snapshot between a full and the
//! next one (when they are at most `max_gap_ticks` apart) moves each entity present in both from
//! the first pose to the second by the playback time's fraction of the gap: a linear blend of the
//! position and a normalized linear blend of the rotation (shortest arc). At a tick that holds a
//! full the blend weight is zero, so whole-tick snapshots are unchanged; entities only in the
//! later full appear when playback reaches it. Blending is presentation only: it is applied to what
//! a bridge shows ([`ReplaySim::presented_snapshot`], [`ExternalSim::read_snapshot`]), never to
//! [`ReplaySim::snapshot`], the playback tick, the events or the state hashes, so `seek`/`step`
//! stay bit-exact with blending on.
//!
//! # Coordinates
//!
//! Moment positions are sim metres and nav positions are centimetres. [`sim_to_regia`] is the
//! one place they convert; it is the identity on axes (DO27's mesh contract is metres, +Y up,
//! forward −Z, `docs/specs/25` § 3) and a test pins it.

use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};

use crate::sim_bridge::{BridgeInput, ExternalSim, SimEntityView, SimSnapshot, SnapshotTick, to_regia_pose};

/// File magic.
pub const REPLAY_MAGIC: [u8; 4] = *b"DO27";
/// Byte length of one Moment body on the wire (`MOMENT_WIRE_BYTES`).
pub const MOMENT_WIRE_BYTES: usize = 89;
/// A Moment more than this many ticks after the previous one opens a new fight (informational:
/// the fight number is stored on the Moment).
pub const FIGHT_GAP_TICKS: u64 = 600;
/// Largest replay file [`ReplayFile::load`] reads.
pub const MAX_REPLAY_BYTES: u64 = 512 * 1024 * 1024;

const TAG_FULL: u8 = 0x01;
const TAG_INPUTS: u8 = 0x02;
const TAG_SALVAGE: u8 = 0x03;
const TAG_NAV: u8 = 0x04;
const TAG_COMMANDS: u8 = 0x05;
const TAG_MOMENTS: u8 = 0x06;
const TAG_ABORT: u8 = 0x07;
const TAG_MAP_EXTENT: u8 = 0x08;
const TAG_TRAINING: u8 = 0x09;

/// Most undrained events kept; older ones are dropped when stepping past it with no drain.
const MAX_PENDING_EVENTS: usize = 100_000;

/// Id offset for entities synthesised from nav beacons, so they never collide with codec ids.
const BEACON_ID_BASE: u64 = 1 << 32;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a replay could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReplayError {
    /// The file ended inside a field.
    #[error("replay truncated at byte {offset}: {needed} more byte(s) needed")]
    Truncated {
        /// Offset of the field that ran out.
        offset: usize,
        /// How many bytes were missing.
        needed: usize,
    },
    /// The first four bytes are not `DO27`.
    #[error("not a DO27 replay (bad magic)")]
    BadMagic,
    /// The version is not 2, 3 or 4.
    #[error("unsupported replay version {0} (this reader accepts 2, 3 and 4)")]
    UnsupportedVersion(u32),
    /// A header string is not UTF-8.
    #[error("string at byte {offset} is not UTF-8")]
    BadString {
        /// Offset of the string's length prefix.
        offset: usize,
    },
    /// A record tag not supported for this version; its body boundary cannot be guessed.
    #[error("unknown record tag {tag:#04x} at byte {offset}")]
    UnknownTag {
        /// The tag byte.
        tag: u8,
        /// Its offset.
        offset: usize,
    },
    /// A version 2 full has no length, so only the writing build's codec can tell where it ends
    /// ([`FullCodec::v2_full_len`]); a player without one refuses the file rather than guess.
    #[error(
        "version 2 full record at byte {0}: a version 2 full has no length prefix, so only the sim build that wrote it can tell where it ends; load it with that build's FullCodec (v2_full_len) or convert the file to version 3"
    )]
    V2Full(usize),
    /// A full record before any applied-inputs record, so it has no tick.
    #[error("full record at byte {0} comes before any applied-inputs record, so it has no tick")]
    FullWithoutTick(usize),
    /// A Moment body is invalid.
    #[error("bad moment at byte {offset}: {reason}")]
    BadMoment {
        /// Offset of the Moment body.
        offset: usize,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A nav body does not match its declared counts.
    #[error("bad nav record at byte {offset}: {reason}")]
    BadNav {
        /// Offset of the nav record.
        offset: usize,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// The file is larger than [`MAX_REPLAY_BYTES`].
    #[error("replay is {size} bytes, over the {max} byte limit")]
    TooLarge {
        /// File size.
        size: u64,
        /// The limit.
        max: u64,
    },
    /// Reading the file failed.
    #[error("cannot read replay: {0}")]
    Io(String),
}

impl ReplayError {
    /// A stable snake_case code for the agent protocol.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Truncated { .. } => "truncated",
            Self::BadMagic => "bad_magic",
            Self::UnsupportedVersion(_) => "unsupported_version",
            Self::BadString { .. } => "bad_string",
            Self::UnknownTag { .. } => "unknown_tag",
            Self::V2Full(_) => "v2_full",
            Self::FullWithoutTick(_) => "full_without_tick",
            Self::BadMoment { .. } => "bad_moment",
            Self::BadNav { .. } => "bad_nav",
            Self::TooLarge { .. } => "too_large",
            Self::Io(_) => "io",
        }
    }
}

// ---------------------------------------------------------------------------
// Coordinates
// ---------------------------------------------------------------------------

/// Converts a position from the replay's sim frame (metres) to Regia Engine's. The identity on axes:
/// DO27's mesh contract is metres, +Y up, forward −Z. This is the only place it happens.
#[inline]
#[must_use]
pub fn sim_to_regia(position: Vec3) -> Vec3 {
    to_regia_pose(position, Quat::IDENTITY).0
}

/// Converts a nav position (integer centimetres in the sim frame) to Regia Engine metres.
#[inline]
#[must_use]
pub fn nav_cm_to_regia(x_cm: i32, y_cm: i32, z_cm: i32) -> Vec3 {
    sim_to_regia(Vec3::new(
        x_cm as f32 / 100.0,
        y_cm as f32 / 100.0,
        z_cm as f32 / 100.0,
    ))
}

// ---------------------------------------------------------------------------
// Parsed model
// ---------------------------------------------------------------------------

/// One row of the player table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlayerRow {
    /// Lockstep seat.
    pub peer_id: u8,
    /// Hub identity key in hex, or empty.
    pub account_id: String,
    /// Display name.
    pub callsign: String,
    /// Peer role wire value (0 client, 1 player host, 2 dedicated host, 3 spectator).
    pub role: u8,
    /// Team index.
    pub team: u8,
}

/// One row of the peer table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerRow {
    /// Lockstep seat.
    pub peer_id: u8,
    /// Peer role wire value.
    pub role: u8,
}

/// The name of a peer role wire value.
#[must_use]
pub fn role_name(role: u8) -> &'static str {
    match role {
        0 => "client",
        1 => "player_host",
        2 => "dedicated_host",
        3 => "spectator",
        _ => "unknown",
    }
}

/// The replay header. Version 2 files fill the version 3 fields with empty or zero.
/// Extra version 4 player/team labels are validated but not retained in this playback header.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplayHeader {
    /// Format version, 2, 3 or 4.
    pub version: u32,
    /// Protocol string of the writing build.
    pub protocol: String,
    /// Sim ticks per second.
    pub tick_rate: u32,
    /// Lobby preset in force.
    pub lobby_preset: String,
    /// Bootstrap seed.
    pub seed: u64,
    /// Battleground id.
    pub bootstrap: String,
    /// Map name.
    pub map: String,
    /// Match mode label.
    pub mode: String,
    /// Team count.
    pub teams: u8,
    /// Player table, in join order.
    pub players: Vec<PlayerRow>,
    /// Lockstep input delay in ticks.
    pub input_delay: u64,
    /// Peer table, in staging order.
    pub peers: Vec<PeerRow>,
    /// Sim build stamp.
    pub sim_git_sha: String,
    /// Sim ABI of that build.
    pub sim_abi: u32,
    /// Map content hash, or empty.
    pub content_hash: String,
    /// Game config hash, or empty.
    pub config_hash: String,
}

/// One pilot's applied input for a tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PilotInput {
    /// Throttle, −127..=127.
    pub throttle: i8,
    /// Strafe.
    pub strafe: i8,
    /// Steer.
    pub steer: i8,
    /// Aim pitch.
    pub aim_pitch: i16,
    /// Action bits.
    pub action_bits: u16,
    /// Stance.
    pub stance: u8,
}

/// A `0x02` applied-inputs record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppliedInputs {
    /// Sim tick.
    pub tick: u32,
    /// `(peer_id, input)` rows, sorted by peer id by the writer.
    pub inputs: Vec<(u8, PilotInput)>,
}

/// A `0x01` full record. The snapshot is opaque (see the module docs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FullRecord {
    /// The tick, taken from the applied-inputs record the full follows.
    pub tick: u32,
    /// The snapshot wire bytes, undecoded.
    pub snapshot: Vec<u8>,
    /// The stored state hash (`None` on version 2).
    pub state_hash: Option<u64>,
}

/// A `0x03` salvage ledger: 4 rows (wrecks made, salvaged, towed, processed) of 5 teams.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SalvageLedger {
    /// Tick.
    pub tick: u32,
    /// Row-major, `rows[row][team]`.
    pub rows: [[u32; 5]; 4],
}

/// A nav beacon.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NavBeacon {
    /// Beacon id.
    pub id: u16,
    /// Owning slot.
    pub owner_slot: u8,
    /// Team.
    pub team: u8,
    /// Beacon number.
    pub number: u8,
    /// X in centimetres.
    pub x_cm: i32,
    /// Y in centimetres.
    pub y_cm: i32,
    /// Z in centimetres.
    pub z_cm: i32,
    /// Health.
    pub health: u16,
}

impl NavBeacon {
    /// The beacon's position in Regia Engine metres.
    #[must_use]
    pub fn position(&self) -> Vec3 {
        nav_cm_to_regia(self.x_cm, self.y_cm, self.z_cm)
    }
}

/// A team waypoint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NavWaypoint {
    /// Team.
    pub team: u8,
    /// Slot.
    pub slot: u8,
    /// Beacon id it points at.
    pub id: u16,
}

/// A `0x04` nav record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NavRecord {
    /// Tick.
    pub tick: u32,
    /// Beacons.
    pub beacons: Vec<NavBeacon>,
    /// Waypoints.
    pub waypoints: Vec<NavWaypoint>,
}

/// One peer command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerCommand {
    /// Issuing seat.
    pub peer_id: u8,
    /// Whether the host accepted it.
    pub ok: bool,
    /// Stable command label (`build`, `order`, `place`, …).
    pub kind: String,
    /// Short debug line.
    pub text: String,
}

/// A `0x05` peer-commands record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerCommands {
    /// Tick.
    pub tick: u32,
    /// Commands in order.
    pub commands: Vec<PeerCommand>,
}

/// The `0x07` abort trailer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AbortRecord {
    /// Tick the match aborted on.
    pub tick: u32,
    /// Why.
    pub reason: String,
}

/// What a Moment records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MomentKind {
    /// A damaging hit (wire 1).
    Hit,
    /// A pilot downed, not dead (wire 2).
    Down,
    /// A hull, pilot or structure destroyed (wire 3).
    Kill,
    /// A kind this reader does not name.
    Other(u8),
}

impl MomentKind {
    /// From the wire value.
    #[must_use]
    pub fn from_wire(value: u8) -> Self {
        match value {
            1 => Self::Hit,
            2 => Self::Down,
            3 => Self::Kill,
            other => Self::Other(other),
        }
    }

    /// The wire value.
    #[must_use]
    pub fn to_wire(self) -> u8 {
        match self {
            Self::Hit => 1,
            Self::Down => 2,
            Self::Kill => 3,
            Self::Other(v) => v,
        }
    }

    /// `hit`, `down`, `kill` or `other`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Down => "down",
            Self::Kill => "kill",
            Self::Other(_) => "other",
        }
    }

    /// Parses a name from [`name`](Self::name), or a decimal wire value.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "hit" => Some(Self::Hit),
            "down" => Some(Self::Down),
            "kill" => Some(Self::Kill),
            other => other.parse::<u8>().ok().map(Self::from_wire),
        }
    }
}

/// One Moment participant slot (slot 0 attacker, slot 1 victim).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Participant {
    /// 0 none, 1 attacker, 2 victim.
    pub role: u8,
    /// Sim entity id.
    pub entity_id: u32,
}

/// A derived combat observation (`0x06` Moments record body).
#[derive(Debug, Clone, PartialEq)]
pub struct Moment {
    /// The tick of the batch this Moment was drained on (the `0x06` record's tick).
    pub drained_tick: u32,
    /// The sim tick stored on the Moment.
    pub tick: u64,
    /// What happened.
    pub kind: MomentKind,
    /// Match span (one sim run is match 1).
    pub span_match: u32,
    /// Round span.
    pub span_round: u32,
    /// Fight span.
    pub span_fight: u32,
    /// Participants, attacker first.
    pub participants: [Participant; 2],
    /// Victim position when the blow landed, in Regia Engine metres.
    pub position: Vec3,
    /// Damage.
    pub damage: u32,
    /// Weapon stem (empty when none).
    pub weapon: String,
    /// Attacker entity id, 0 for none.
    pub attacker: u32,
    /// Victim entity id.
    pub victim: u32,
    /// Importance hint 0..=100, 0 unset.
    pub importance: u8,
}

/// A filter over [`ReplayFile::moments`]. Unset fields match everything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MomentQuery {
    /// Only this kind.
    pub kind: Option<MomentKind>,
    /// Stored tick at or after this.
    pub from_tick: Option<u64>,
    /// Stored tick at or before this.
    pub to_tick: Option<u64>,
    /// This entity as attacker, victim or in either participant slot.
    pub entity: Option<u32>,
    /// This entity as attacker.
    pub attacker: Option<u32>,
    /// This entity as victim.
    pub victim: Option<u32>,
    /// This fight span.
    pub fight: Option<u32>,
    /// This round span.
    pub round: Option<u32>,
    /// Weapon stem equal to this.
    pub weapon: Option<String>,
    /// Importance at least this.
    pub min_importance: u8,
}

impl MomentQuery {
    /// Whether `moment` passes every set filter.
    #[must_use]
    pub fn matches(&self, moment: &Moment) -> bool {
        if self.kind.is_some_and(|k| k != moment.kind) {
            return false;
        }
        if self.from_tick.is_some_and(|t| moment.tick < t) || self.to_tick.is_some_and(|t| moment.tick > t) {
            return false;
        }
        if let Some(entity) = self.entity {
            let in_slot = moment
                .participants
                .iter()
                .any(|p| p.role != 0 && p.entity_id == entity);
            if !(in_slot || moment.attacker == entity || moment.victim == entity) {
                return false;
            }
        }
        if self.attacker.is_some_and(|a| moment.attacker != a)
            || self.victim.is_some_and(|v| moment.victim != v)
        {
            return false;
        }
        if self.fight.is_some_and(|f| moment.span_fight != f)
            || self.round.is_some_and(|r| moment.span_round != r)
        {
            return false;
        }
        if self.weapon.as_ref().is_some_and(|w| &moment.weapon != w) {
            return false;
        }
        moment.importance >= self.min_importance
    }
}

/// A parsed replay.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReplayFile {
    /// The header.
    pub header: ReplayHeader,
    /// Applied-inputs records, in file order.
    pub inputs: Vec<AppliedInputs>,
    /// Fulls, in file order.
    pub fulls: Vec<FullRecord>,
    /// Salvage ledgers.
    pub salvage: Vec<SalvageLedger>,
    /// Nav records.
    pub navs: Vec<NavRecord>,
    /// Peer-command records.
    pub commands: Vec<PeerCommands>,
    /// Every Moment, in emission (file) order. The index into this vector is the Moment's id.
    pub moments: Vec<Moment>,
    /// Number of `0x06` records (ticks that emitted Moments).
    pub moment_batches: usize,
    /// The abort trailer, when the match aborted.
    pub abort: Option<AbortRecord>,
    /// Size of the parsed file in bytes.
    pub byte_len: usize,
}

// ---------------------------------------------------------------------------
// Byte reader
// ---------------------------------------------------------------------------

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ReplayError> {
        if n > self.remaining() {
            return Err(ReplayError::Truncated {
                offset: self.pos,
                needed: n - self.remaining(),
            });
        }
        let slice = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], ReplayError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, ReplayError> {
        Ok(self.array::<1>()?[0])
    }

    fn i8(&mut self) -> Result<i8, ReplayError> {
        Ok(i8::from_le_bytes(self.array()?))
    }

    fn u16(&mut self) -> Result<u16, ReplayError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn i16(&mut self) -> Result<i16, ReplayError> {
        Ok(i16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, ReplayError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn i32(&mut self) -> Result<i32, ReplayError> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, ReplayError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn f32(&mut self) -> Result<f32, ReplayError> {
        Ok(f32::from_le_bytes(self.array()?))
    }

    /// A `u16`-length-prefixed UTF-8 string.
    fn string(&mut self) -> Result<String, ReplayError> {
        let offset = self.pos;
        let len = usize::from(self.u16()?);
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| ReplayError::BadString { offset })
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

impl ReplayFile {
    /// Parses a replay from memory. Never panics; every malformed input is an error. A version 2
    /// full is refused ([`ReplayError::V2Full`]); [`Self::parse_with`] reads it with a codec.
    pub fn parse(data: &[u8]) -> Result<Self, ReplayError> {
        Self::parse_with(data, &OpaqueCodec)
    }

    /// [`Self::parse`], asking `codec` where a version 2 full ends ([`FullCodec::v2_full_len`]).
    /// A codec that cannot tell leaves the file refused with [`ReplayError::V2Full`]; version 3 and 4
    /// files parse the same with any codec.
    pub fn parse_with(data: &[u8], codec: &dyn FullCodec) -> Result<Self, ReplayError> {
        let mut r = Reader::new(data);
        if r.array::<4>()? != REPLAY_MAGIC {
            return Err(ReplayError::BadMagic);
        }
        let version = r.u32()?;
        if !(2..=4).contains(&version) {
            return Err(ReplayError::UnsupportedVersion(version));
        }
        let mut header = ReplayHeader {
            version,
            protocol: r.string()?,
            tick_rate: r.u32()?,
            lobby_preset: r.string()?,
            ..ReplayHeader::default()
        };
        if version >= 3 {
            header.seed = r.u64()?;
            header.bootstrap = r.string()?;
            header.map = r.string()?;
            header.mode = r.string()?;
            header.teams = r.u8()?;
            let players = r.u8()?;
            for _ in 0..players {
                header.players.push(PlayerRow {
                    peer_id: r.u8()?,
                    account_id: r.string()?,
                    callsign: r.string()?,
                    role: r.u8()?,
                    team: r.u8()?,
                });
                if version == 4 {
                    // Presentation does not use the extra label, but its wire string must be valid.
                    r.string()?;
                }
            }
            header.input_delay = r.u64()?;
            let peers = r.u8()?;
            for _ in 0..peers {
                header.peers.push(PeerRow {
                    peer_id: r.u8()?,
                    role: r.u8()?,
                });
            }
            header.sim_git_sha = r.string()?;
            header.sim_abi = r.u32()?;
            header.content_hash = r.string()?;
            header.config_hash = r.string()?;
            if version == 4 {
                let labels = r.u8()?;
                for _ in 0..labels {
                    r.string()?;
                }
            }
        }

        let mut file = ReplayFile {
            header,
            byte_len: data.len(),
            ..ReplayFile::default()
        };
        let mut last_input_tick: Option<u32> = None;
        while r.remaining() > 0 {
            let at = r.pos;
            let tag = r.u8()?;
            match tag {
                TAG_FULL if version == 2 => {
                    // No length prefix: the body runs to wherever the writing build's layout
                    // says. Without a codec that knows, refuse instead of guessing a boundary.
                    let body_len = codec.v2_full_len(&data[r.pos..]).ok_or(ReplayError::V2Full(at))?;
                    let tick = last_input_tick.ok_or(ReplayError::FullWithoutTick(at))?;
                    let snapshot = r.take(body_len)?.to_vec();
                    file.fulls.push(FullRecord {
                        tick,
                        snapshot,
                        state_hash: None,
                    });
                }
                TAG_FULL => {
                    let tick = last_input_tick.ok_or(ReplayError::FullWithoutTick(at))?;
                    let body_len = r.u32()? as usize;
                    let snapshot = r.take(body_len)?.to_vec();
                    let state_hash = Some(r.u64()?);
                    file.fulls.push(FullRecord {
                        tick,
                        snapshot,
                        state_hash,
                    });
                }
                TAG_INPUTS => {
                    let tick = r.u32()?;
                    let count = r.u8()?;
                    let mut inputs = Vec::with_capacity(usize::from(count));
                    for _ in 0..count {
                        let peer = r.u8()?;
                        inputs.push((
                            peer,
                            PilotInput {
                                throttle: r.i8()?,
                                strafe: r.i8()?,
                                steer: r.i8()?,
                                aim_pitch: r.i16()?,
                                action_bits: r.u16()?,
                                stance: r.u8()?,
                            },
                        ));
                    }
                    last_input_tick = Some(tick);
                    file.inputs.push(AppliedInputs { tick, inputs });
                }
                TAG_SALVAGE => {
                    let tick = r.u32()?;
                    let mut rows = [[0u32; 5]; 4];
                    for row in &mut rows {
                        for cell in row.iter_mut() {
                            *cell = r.u32()?;
                        }
                    }
                    file.salvage.push(SalvageLedger { tick, rows });
                }
                TAG_NAV => {
                    let tick = r.u32()?;
                    let body_len = r.u32()? as usize;
                    let body = r.take(body_len)?;
                    file.navs.push(parse_nav(tick, body, at)?);
                }
                TAG_COMMANDS => {
                    let tick = r.u32()?;
                    let count = r.u8()?;
                    let mut commands = Vec::with_capacity(usize::from(count));
                    for _ in 0..count {
                        let peer_id = r.u8()?;
                        let ok = r.u8()? != 0;
                        let kind_len = usize::from(r.u8()?);
                        let kind = String::from_utf8_lossy(r.take(kind_len)?).into_owned();
                        let text_len = usize::from(r.u16()?);
                        let text = String::from_utf8_lossy(r.take(text_len)?).into_owned();
                        commands.push(PeerCommand {
                            peer_id,
                            ok,
                            kind,
                            text,
                        });
                    }
                    file.commands.push(PeerCommands { tick, commands });
                }
                TAG_MOMENTS => {
                    let tick = r.u32()?;
                    let count = usize::from(r.u16()?);
                    let body_at = r.pos;
                    let bodies = r.take(count * MOMENT_WIRE_BYTES)?;
                    for (i, body) in bodies.as_chunks::<MOMENT_WIRE_BYTES>().0.iter().enumerate() {
                        file.moments
                            .push(parse_moment(tick, body, body_at + i * MOMENT_WIRE_BYTES)?);
                    }
                    file.moment_batches += 1;
                }
                TAG_ABORT => {
                    let tick = r.u32()?;
                    let reason = r.string()?;
                    file.abort = Some(AbortRecord { tick, reason });
                }
                TAG_MAP_EXTENT if version == 4 => {
                    // Four 32-bit fields; no playback state or input tick is supplied by this record.
                    r.take(16)?;
                }
                TAG_TRAINING if version == 4 => {
                    // Skip the envelope's tick and opaque body, never scanning its bytes for tags.
                    r.u32()?;
                    let body_len = r.u32()? as usize;
                    r.take(body_len)?;
                }
                tag => return Err(ReplayError::UnknownTag { tag, offset: at }),
            }
        }
        Ok(file)
    }

    /// Reads and parses the file at `path` (at most [`MAX_REPLAY_BYTES`]).
    ///
    /// One descriptor is opened and used for the size check and the read; only regular files are
    /// accepted (no FIFOs, devices or directories), and the read is bounded by
    /// [`MAX_REPLAY_BYTES`] even if the file grows after the size check.
    pub fn load(path: &Path) -> Result<Self, ReplayError> {
        Self::load_with(path, &OpaqueCodec)
    }

    /// [`Self::load`], reading version 2 fulls with `codec` ([`Self::parse_with`]).
    pub fn load_with(path: &Path, codec: &dyn FullCodec) -> Result<Self, ReplayError> {
        use std::io::Read as _;
        let file = std::fs::File::open(path).map_err(|e| ReplayError::Io(e.to_string()))?;
        let meta = file.metadata().map_err(|e| ReplayError::Io(e.to_string()))?;
        if !meta.is_file() {
            return Err(ReplayError::Io("not a regular file".into()));
        }
        if meta.len() > MAX_REPLAY_BYTES {
            return Err(ReplayError::TooLarge {
                size: meta.len(),
                max: MAX_REPLAY_BYTES,
            });
        }
        let mut data = Vec::new();
        // One byte past the cap distinguishes "exactly at the cap" from "grew past it".
        file.take(MAX_REPLAY_BYTES + 1)
            .read_to_end(&mut data)
            .map_err(|e| ReplayError::Io(e.to_string()))?;
        if data.len() as u64 > MAX_REPLAY_BYTES {
            return Err(ReplayError::TooLarge {
                size: data.len() as u64,
                max: MAX_REPLAY_BYTES,
            });
        }
        Self::parse_with(&data, codec)
    }
}

fn parse_nav(tick: u32, body: &[u8], record_at: usize) -> Result<NavRecord, ReplayError> {
    let bad = |reason| ReplayError::BadNav {
        offset: record_at,
        reason,
    };
    let mut r = Reader::new(body);
    let beacon_count = r.u16().map_err(|_| bad("body shorter than its beacon count"))?;
    let mut beacons = Vec::new();
    for _ in 0..beacon_count {
        let beacon = (|| {
            Ok::<_, ReplayError>(NavBeacon {
                id: r.u16()?,
                owner_slot: r.u8()?,
                team: r.u8()?,
                number: r.u8()?,
                x_cm: r.i32()?,
                y_cm: r.i32()?,
                z_cm: r.i32()?,
                health: r.u16()?,
            })
        })()
        .map_err(|_| bad("body ends inside a beacon"))?;
        beacons.push(beacon);
    }
    let waypoint_count = r.u16().map_err(|_| bad("body has no waypoint count"))?;
    let mut waypoints = Vec::new();
    for _ in 0..waypoint_count {
        let waypoint = (|| {
            Ok::<_, ReplayError>(NavWaypoint {
                team: r.u8()?,
                slot: r.u8()?,
                id: r.u16()?,
            })
        })()
        .map_err(|_| bad("body ends inside a waypoint"))?;
        waypoints.push(waypoint);
    }
    if r.remaining() != 0 {
        return Err(bad("body has bytes past its waypoints"));
    }
    Ok(NavRecord {
        tick,
        beacons,
        waypoints,
    })
}

fn parse_moment(drained_tick: u32, body: &[u8], at: usize) -> Result<Moment, ReplayError> {
    let bad = |reason| ReplayError::BadMoment { offset: at, reason };
    let mut r = Reader::new(body);
    let tick = r.u64()?;
    let kind = MomentKind::from_wire(r.u8()?);
    let span_match = r.u32()?;
    let span_round = r.u32()?;
    let span_fight = r.u32()?;
    let mut participants = [Participant::default(); 2];
    for slot in &mut participants {
        slot.role = r.u8()?;
        slot.entity_id = r.u32()?;
    }
    let position = sim_to_regia(Vec3::new(r.f32()?, r.f32()?, r.f32()?));
    let damage = r.u32()?;
    let weapon_len = usize::from(r.u8()?);
    let weapon_field = r.take(32)?;
    if weapon_len > 32 {
        return Err(bad("weapon length above 32"));
    }
    let weapon = std::str::from_utf8(&weapon_field[..weapon_len])
        .map_err(|_| bad("weapon stem is not UTF-8"))?
        .to_owned();
    let attacker = r.u32()?;
    let victim = r.u32()?;
    let importance = r.u8()?;
    Ok(Moment {
        drained_tick,
        tick,
        kind,
        span_match,
        span_round,
        span_fight,
        participants,
        position,
        damage,
        weapon,
        attacker,
        victim,
        importance,
    })
}

// ---------------------------------------------------------------------------
// Queries and verification
// ---------------------------------------------------------------------------

impl ReplayFile {
    /// The last tick any record names: playback runs `0..=end_tick()`.
    #[must_use]
    pub fn end_tick(&self) -> u32 {
        let mut end = 0u32;
        end = self.inputs.iter().map(|r| r.tick).fold(end, u32::max);
        end = self.fulls.iter().map(|r| r.tick).fold(end, u32::max);
        end = self.navs.iter().map(|r| r.tick).fold(end, u32::max);
        end = self.commands.iter().map(|r| r.tick).fold(end, u32::max);
        end = self.salvage.iter().map(|r| r.tick).fold(end, u32::max);
        end = self.moments.iter().map(|m| m.drained_tick).fold(end, u32::max);
        if let Some(abort) = &self.abort {
            end = end.max(abort.tick);
        }
        end
    }

    /// Playback length in seconds at the header's tick rate (0 when the rate is 0).
    #[must_use]
    pub fn duration_secs(&self) -> f64 {
        if self.header.tick_rate == 0 {
            0.0
        } else {
            f64::from(self.end_tick()) / f64::from(self.header.tick_rate)
        }
    }

    /// Indexes into [`moments`](Self::moments) of every Moment passing `query`, in emission order.
    #[must_use]
    pub fn moments_matching(&self, query: &MomentQuery) -> Vec<usize> {
        self.moments
            .iter()
            .enumerate()
            .filter(|(_, m)| query.matches(m))
            .map(|(i, _)| i)
            .collect()
    }

    /// The applied inputs recorded for `tick` (the last such record when a defective file has
    /// two).
    #[must_use]
    pub fn inputs_at(&self, tick: u32) -> Option<&AppliedInputs> {
        self.inputs.iter().rev().find(|r| r.tick == tick)
    }

    /// The structural and (with a hashing codec) state-hash audit.
    ///
    /// Mirrors `ironreach_replay --verify` as far as an outside reader can: at least one full,
    /// non-decreasing input ticks, an applied-inputs record on every full's tick, no duplicate
    /// `peer_id` in either table. When `codec` can recompute a full's hash it must equal the
    /// stored one bit for bit; otherwise the full counts as *unchecked*, never as passed.
    #[must_use]
    pub fn verify(&self, codec: &dyn FullCodec) -> VerifyReport {
        let mut report = VerifyReport {
            fulls: self.fulls.len(),
            moment_ticks: self.moment_batches,
            aborted: self.abort.is_some(),
            ..VerifyReport::default()
        };
        if self.fulls.is_empty() {
            report.defects.push("no full record".to_owned());
        }
        if let Some(pair) = self.inputs.windows(2).find(|w| w[1].tick < w[0].tick) {
            report.defects.push(format!(
                "applied-inputs ticks decrease ({} then {})",
                pair[0].tick, pair[1].tick
            ));
        }
        for full in &self.fulls {
            if self.inputs_at(full.tick).is_none() {
                report
                    .defects
                    .push(format!("full at tick {} has no applied-inputs record", full.tick));
            }
            match (full.state_hash, codec.state_hash(&full.snapshot)) {
                (Some(stored), Some(recomputed)) => {
                    report.hash_checked += 1;
                    if stored != recomputed {
                        report.mismatches.push(full.tick);
                    }
                }
                _ => report.hash_unchecked += 1,
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for row in &self.header.players {
            if !seen.insert(row.peer_id) {
                report
                    .defects
                    .push(format!("duplicate peer_id {} in the player table", row.peer_id));
            }
        }
        seen.clear();
        for row in &self.header.peers {
            if !seen.insert(row.peer_id) {
                report
                    .defects
                    .push(format!("duplicate peer_id {} in the peer table", row.peer_id));
            }
        }
        report
    }
}

/// What [`ReplayFile::verify`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    /// Number of fulls.
    pub fulls: usize,
    /// Fulls whose hash was recomputed and compared.
    pub hash_checked: usize,
    /// Fulls that could not be checked (no hashing codec, or a version 2 file).
    pub hash_unchecked: usize,
    /// Ticks of fulls whose recomputed hash differs from the stored one.
    pub mismatches: Vec<u32>,
    /// Structural defects, one line each.
    pub defects: Vec<String>,
    /// Number of `0x06` records.
    pub moment_ticks: usize,
    /// Whether the abort trailer is present (reported, not a failure).
    pub aborted: bool,
}

impl VerifyReport {
    /// No mismatch and no defect.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.mismatches.is_empty() && self.defects.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Codec: the seam to the matching sim build
// ---------------------------------------------------------------------------

/// One render-ready entity decoded from a full.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayEntity {
    /// Stable id.
    pub id: u64,
    /// Kind key (for example a DO27 unit stem).
    ///
    /// An `Arc<str>` so a presented snapshot shares the decoded row's allocation instead of
    /// copying the text every frame. `Arc` clones bump a count; they do not allocate the bytes.
    pub kind: Arc<str>,
    /// Team index.
    pub team: u32,
    /// Position in Regia Engine metres (the codec converts with [`sim_to_regia`]).
    pub translation: Vec3,
    /// Rotation in Regia Engine's frame.
    pub rotation: Quat,
}

/// Decodes the opaque snapshot of a full. Implemented by the game against the sim build that
/// wrote the replay; the engine ships only [`OpaqueCodec`].
pub trait FullCodec: Send + Sync + 'static {
    /// The entities in `snapshot`, or why it could not be decoded.
    fn decode(&self, snapshot: &[u8]) -> Result<Vec<ReplayEntity>, String>;

    /// The state hash of `snapshot` as the writing sim computes it, or `None` when this codec
    /// cannot recompute it.
    fn state_hash(&self, snapshot: &[u8]) -> Option<u64>;

    /// Where a version 2 full ends. A version 2 full (`0x01`) has no length prefix: `body` is the
    /// rest of the file after the tag byte, and only the writing build's layout knows how many of
    /// those bytes are the snapshot. Return that count (the next record starts right after it),
    /// or `None` when this codec cannot tell; the default is `None`, and the file is then refused
    /// with [`ReplayError::V2Full`]. A version 2 full carries no state hash, so `verify` counts
    /// it as unchecked.
    fn v2_full_len(&self, _body: &[u8]) -> Option<usize> {
        None
    }
}

/// The codec for outside readers: decodes nothing (every decode is an `Err`, so a snapshot
/// reports `decode_error` instead of an empty entity list) and checks no hash.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpaqueCodec;

impl FullCodec for OpaqueCodec {
    fn decode(&self, _snapshot: &[u8]) -> Result<Vec<ReplayEntity>, String> {
        Err("no_full_codec: this player has no codec for full snapshots".to_owned())
    }

    fn state_hash(&self, _snapshot: &[u8]) -> Option<u64> {
        None
    }
}

// ---------------------------------------------------------------------------
// ExternalSim
// ---------------------------------------------------------------------------

/// The render-ready entity set at the playback tick.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReplaySnapshot {
    /// The playback tick.
    pub tick: u32,
    /// Tick of the full the entities come from, if any.
    pub full_tick: Option<u32>,
    /// Tick of the nav record the beacons come from, if any.
    pub nav_tick: Option<u32>,
    /// Entities from the codec, then one `"beacon"` entity per nav beacon.
    pub entities: Vec<ReplayEntity>,
    /// Set when the codec could not decode the current full.
    pub decode_error: Option<String>,
    /// Set when the entities are blended toward the next full (sub-tick interpolation,
    /// [`ReplaySim::set_interpolation`]); only a [`ReplaySim::presented_snapshot`] blends.
    pub blend: Option<ReplayBlend>,
}

/// How a blended [`ReplaySnapshot`] sits between two fulls.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReplayBlend {
    /// Tick of the full the entities move toward.
    pub next_full_tick: u32,
    /// Weight of that full, in `(0, 1)`: the playback time's fraction of the gap.
    pub alpha: f32,
}

/// Most ticks between two fulls that sub-tick blending interpolates across.
pub const MAX_BLEND_GAP_TICKS: u32 = 600;

impl SimSnapshot for ReplaySnapshot {
    fn for_each_entity(&self, mut f: impl FnMut(SimEntityView<'_>)) {
        for entity in &self.entities {
            f(SimEntityView {
                id: entity.id,
                kind: &entity.kind,
                team: entity.team,
                translation: entity.translation,
                rotation: entity.rotation,
            });
        }
    }
}

/// Something that happened as playback crossed a tick forward.
#[derive(Debug, Clone, PartialEq)]
pub enum ReplayEvent {
    /// A Moment was drained on this tick.
    Moment(Moment),
    /// A peer command was applied on this tick.
    Command {
        /// Tick.
        tick: u32,
        /// The command.
        command: PeerCommand,
    },
    /// The match aborted on this tick.
    Abort(AbortRecord),
}

#[derive(Debug, Clone, Copy)]
enum EventSrc {
    Moment(usize),
    Command(usize, usize),
    Abort,
}

/// A deterministic player for a [`ReplayFile`], usable as the sim of a
/// [`SimBridgePlugin`](crate::sim_bridge::SimBridgePlugin) and as a plain resource for headless
/// agent work (`replay.*`, `moments.*`).
#[derive(Resource)]
pub struct ReplaySim {
    file: Option<ReplayFile>,
    codec: Box<dyn FullCodec>,
    tick: u32,
    /// Events up to and including this tick have been delivered (or skipped by a seek).
    cursor: u32,
    /// True from a load until the first step or seek: the events stamped tick 0 (before any tick
    /// is crossed) are still owed to the first step.
    fresh: bool,
    pending: Vec<ReplayEvent>,
    /// How many of the newest `pending` events the last [`step`](Self::step) queued.
    last_step_events: usize,
    playing: bool,
    speed_permille: u32,
    /// Fractional playback time in units of `1 / 1_000_000_000` tick.
    acc: u128,
    /// Frame time still to be absorbed without playing ([`ReplaySim::hold_clock`]).
    hold: Duration,
    /// Sub-tick blending across fulls at most this many ticks apart; 0 is off.
    blend_gap: u32,
    events: Vec<(u32, EventSrc)>,
    decoded: Option<(usize, Result<Vec<ReplayEntity>, String>)>,
    /// The decode of the full a blended snapshot moves toward.
    decoded_next: Option<(usize, Result<Vec<ReplayEntity>, String>)>,
    /// Last full located for a tick: `(tick, index of the last full at or before it)`.
    full_cursor: Option<(u32, usize)>,
    /// Last nav record located for a tick, same shape as `full_cursor`.
    nav_cursor: Option<(u32, usize)>,
    /// Unblended decoded rows for one full. Rebuilt only when that full index changes.
    /// Beacons are appended after blending, so they keep the pose the nav record stored.
    entity_rows: Vec<ReplayEntity>,
    /// The full index `entity_rows` was built for. The outer `None` means the rows are stale;
    /// `Some(None)` is a real build with no full at the tick.
    entity_key: Option<Option<usize>>,
    /// Id-to-row map of `decoded_next`, keyed by that full's index.
    blend_ids: Option<(usize, std::collections::BTreeMap<u64, usize>)>,
}

impl Default for ReplaySim {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ReplaySim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplaySim")
            .field("loaded", &self.file.is_some())
            .field("tick", &self.tick)
            .field("playing", &self.playing)
            .finish_non_exhaustive()
    }
}

impl ReplaySim {
    /// An empty player with the [`OpaqueCodec`]. Load a replay with [`load`](Self::load).
    #[must_use]
    pub fn new() -> Self {
        Self::with_codec(OpaqueCodec)
    }

    /// An empty player that decodes fulls with `codec`.
    #[must_use]
    pub fn with_codec(codec: impl FullCodec) -> Self {
        Self {
            file: None,
            codec: Box::new(codec),
            tick: 0,
            cursor: 0,
            fresh: false,
            pending: Vec::new(),
            last_step_events: 0,
            playing: false,
            speed_permille: 1000,
            acc: 0,
            hold: Duration::ZERO,
            blend_gap: 0,
            events: Vec::new(),
            decoded: None,
            decoded_next: None,
            full_cursor: None,
            nav_cursor: None,
            entity_rows: Vec::new(),
            entity_key: None,
            blend_ids: None,
        }
    }

    /// Replaces the codec (the loaded replay stays).
    pub fn set_codec(&mut self, codec: impl FullCodec) {
        self.codec = Box::new(codec);
        self.decoded = None;
        self.decoded_next = None;
        self.entity_rows.clear();
        self.entity_key = None;
        self.blend_ids = None;
    }

    /// The codec, for [`ReplayFile::verify`].
    #[must_use]
    pub fn codec(&self) -> &dyn FullCodec {
        self.codec.as_ref()
    }

    /// Takes ownership of `file` and rewinds to tick 0, paused.
    pub fn load(&mut self, file: ReplayFile) {
        let mut events = Vec::new();
        for (i, moment) in file.moments.iter().enumerate() {
            events.push((moment.drained_tick, EventSrc::Moment(i)));
        }
        for (i, record) in file.commands.iter().enumerate() {
            for j in 0..record.commands.len() {
                events.push((record.tick, EventSrc::Command(i, j)));
            }
        }
        if let Some(abort) = &file.abort {
            events.push((abort.tick, EventSrc::Abort));
        }
        // Stable: within a tick the order is Moments, then commands, then the abort.
        events.sort_by_key(|(tick, _)| *tick);
        self.events = events;
        self.file = Some(file);
        self.tick = 0;
        self.cursor = 0;
        self.fresh = true;
        self.pending.clear();
        self.last_step_events = 0;
        self.playing = false;
        self.acc = 0;
        self.hold = Duration::ZERO;
        self.decoded = None;
        self.decoded_next = None;
        self.full_cursor = None;
        self.nav_cursor = None;
        self.entity_rows.clear();
        self.entity_key = None;
        self.blend_ids = None;
    }

    /// Parses `data` and loads it (version 2 fulls are read with this player's codec,
    /// [`ReplayFile::parse_with`]).
    pub fn load_bytes(&mut self, data: &[u8]) -> Result<(), ReplayError> {
        let file = ReplayFile::parse_with(data, self.codec.as_ref())?;
        self.load(file);
        Ok(())
    }

    /// Reads, parses and loads the file at `path` (version 2 fulls are read with this player's
    /// codec, [`ReplayFile::load_with`]).
    pub fn load_path(&mut self, path: &Path) -> Result<(), ReplayError> {
        let file = ReplayFile::load_with(path, self.codec.as_ref())?;
        self.load(file);
        Ok(())
    }

    /// The loaded replay.
    #[must_use]
    pub fn file(&self) -> Option<&ReplayFile> {
        self.file.as_ref()
    }

    /// The playback tick.
    #[must_use]
    pub fn tick(&self) -> u32 {
        self.tick
    }

    /// The last playable tick (0 with nothing loaded).
    #[must_use]
    pub fn end_tick(&self) -> u32 {
        self.file.as_ref().map_or(0, ReplayFile::end_tick)
    }

    /// Whether the frame clock advances playback.
    #[must_use]
    pub fn playing(&self) -> bool {
        self.playing
    }

    /// Starts or stops frame-clock playback ([`ExternalSim::read_snapshot`]). Stepping and
    /// seeking work either way.
    pub fn set_playing(&mut self, playing: bool) {
        self.playing = playing;
    }

    /// Playback speed in thousandths (1000 = real time, 0 pauses the clock).
    pub fn set_speed_permille(&mut self, speed: u32) {
        self.speed_permille = speed;
    }

    /// The playback speed in thousandths.
    #[must_use]
    pub fn speed_permille(&self) -> u32 {
        self.speed_permille
    }

    /// The playback time past [`Self::tick`], as a fraction of a tick in `[0, 1)`.
    #[must_use]
    pub fn tick_fraction(&self) -> f64 {
        self.acc as f64 / 1e9
    }

    /// The next `frame_time` of frame-clock playback is absorbed without playing (replacing any
    /// hold not yet used up). A render-out that starts a replay on its frame 0 holds the step into
    /// that frame, so the frame shows the stated tick ([`Self::advance_clock`]).
    pub fn hold_clock(&mut self, frame_time: Duration) {
        self.hold = frame_time;
    }

    /// Frame time still held ([`Self::hold_clock`]).
    #[must_use]
    pub fn held(&self) -> Duration {
        self.hold
    }

    /// Turns sub-tick blending on for fulls at most `max_gap_ticks` apart (clamped to
    /// [`MAX_BLEND_GAP_TICKS`]), or off with 0 (the default). With 1, only fulls on consecutive
    /// ticks blend, so the blend is purely between ticks.
    pub fn set_interpolation(&mut self, max_gap_ticks: u32) {
        self.blend_gap = max_gap_ticks.min(MAX_BLEND_GAP_TICKS);
        self.decoded_next = None;
        self.blend_ids = None;
    }

    /// The blend gap set by [`Self::set_interpolation`] (0 = off).
    #[must_use]
    pub fn interpolation(&self) -> u32 {
        self.blend_gap
    }

    /// Moves to `tick` (clamped to the end) without delivering any event, forwards or back.
    /// Returns the tick reached.
    pub fn seek(&mut self, tick: u32) -> u32 {
        self.tick = tick.min(self.end_tick());
        self.cursor = self.tick;
        self.fresh = false;
        self.pending.clear();
        self.last_step_events = 0;
        self.acc = 0;
        self.tick
    }

    /// Advances `ticks` ticks (stopping at the end), queueing the events of every tick crossed
    /// for [`drain_events`](ExternalSim::drain_events). Returns the ticks actually advanced.
    pub fn step(&mut self, ticks: u32) -> u32 {
        let from = self.tick;
        let to = from.saturating_add(ticks).min(self.end_tick());
        self.tick = to;
        // Headless users who never drain must not grow the queue without bound.
        if self.pending.len() > MAX_PENDING_EVENTS {
            self.pending
                .drain(..self.pending.len() - MAX_PENDING_EVENTS / 2)
                .for_each(drop);
        }
        let queued = self.pending.len();
        let include_start = std::mem::take(&mut self.fresh);
        if to > self.cursor || include_start {
            self.collect_events(self.cursor, to, include_start);
            self.cursor = to;
        }
        self.last_step_events = self.pending.len() - queued;
        if to >= self.end_tick() {
            self.playing = false;
        }
        to - from
    }

    /// The events the last [`step`](Self::step) queued (also still pending for the next drain).
    #[must_use]
    pub fn last_step_events(&self) -> &[ReplayEvent] {
        &self.pending[self.pending.len() - self.last_step_events.min(self.pending.len())..]
    }

    /// Queues the events with `after < tick <= through` (`after <= tick` when `include_start`,
    /// for the first step after a load). The event list is sorted by tick.
    fn collect_events(&mut self, after: u32, through: u32, include_start: bool) {
        let Some(file) = &self.file else { return };
        let first = if include_start {
            self.events.partition_point(|(tick, _)| *tick < after)
        } else {
            self.events.partition_point(|(tick, _)| *tick <= after)
        };
        let last = self.events.partition_point(|(tick, _)| *tick <= through);
        for &(tick, src) in &self.events[first..last] {
            let event = match src {
                EventSrc::Moment(i) => file.moments.get(i).cloned().map(ReplayEvent::Moment),
                EventSrc::Command(i, j) => file
                    .commands
                    .get(i)
                    .and_then(|record| record.commands.get(j))
                    .cloned()
                    .map(|command| ReplayEvent::Command { tick, command }),
                EventSrc::Abort => file.abort.clone().map(ReplayEvent::Abort),
            };
            self.pending.extend(event);
        }
    }

    /// Advances the playback clock by `frame_time` (when playing; [`ExternalSim::read_snapshot`]
    /// calls it with the frame's game delta). A [`Self::hold_clock`] is used up first. Exact
    /// integer arithmetic.
    pub fn advance_clock(&mut self, frame_time: Duration) {
        if !self.playing || self.speed_permille == 0 {
            return;
        }
        let Some(file) = &self.file else { return };
        let held = self.hold.min(frame_time);
        self.hold -= held;
        let frame_time = frame_time - held;
        let rate = u128::from(file.header.tick_rate);
        self.acc += frame_time.as_nanos() * rate * u128::from(self.speed_permille) / 1000;
        let whole = self.acc / 1_000_000_000;
        self.acc %= 1_000_000_000;
        self.step(u32::try_from(whole).unwrap_or(u32::MAX));
    }

    /// The entity set at the playback tick: the codec's decode of the last full at or before it,
    /// plus one `"beacon"` entity per beacon of the last nav record at or before it. Exact: it
    /// never blends (sub-tick blending is presentation only, [`Self::presented_snapshot`]), so
    /// [`Self::seek`] and [`Self::step`] answers, and anything hashed from them, do not depend on
    /// [`Self::set_interpolation`] or the frame clock's fraction.
    pub fn snapshot(&mut self) -> ReplaySnapshot {
        self.snapshot_with(false)
    }

    /// What a bridge shows this frame ([`ExternalSim::read_snapshot`]): [`Self::snapshot`], with
    /// the entities blended toward the next full when sub-tick blending is on
    /// ([`Self::set_interpolation`], ADR-1519). Presentation only: the playback tick, the events
    /// and [`Self::snapshot`] are the same with blending on or off.
    pub fn presented_snapshot(&mut self) -> ReplaySnapshot {
        self.snapshot_with(self.blend_gap > 0)
    }

    fn snapshot_with(&mut self, blend: bool) -> ReplaySnapshot {
        let _span = tracing::info_span!(target: "regia_framework", "replay_snapshot").entered();
        let tick = self.tick;
        let mut out = ReplaySnapshot {
            tick,
            ..ReplaySnapshot::default()
        };
        if self.file.is_none() {
            return out;
        }
        let full_index = self.locate_full(tick);
        let nav_index = self.locate_nav(tick);
        if let Some(index) = full_index {
            self.decode_full(index);
            if let Some(full_tick) = self.full_tick_at(index) {
                out.full_tick = Some(full_tick);
            }
            if let Some(error) = self.decode_error_at(index) {
                out.decode_error = Some(error);
            }
        }
        self.ensure_entity_rows(full_index);
        out.entities.clone_from(&self.entity_rows);
        if let Some(nav_tick) = nav_index.and_then(|index| self.nav_tick_at(index)) {
            out.nav_tick = Some(nav_tick);
        }
        if blend
            && out.decode_error.is_none()
            && let Some((next_index, next_tick, full_tick)) = self.blend_target(full_index, tick)
        {
            let gap = f64::from(next_tick - full_tick);
            let alpha = (f64::from(tick - full_tick) + self.tick_fraction()) / gap;
            if alpha > 0.0 {
                self.ensure_blend_map(next_index);
                // A failed decode of the next full leaves the previous map (or none) and
                // publishes no blend, matching a snapshot that only blends `Ok` targets.
                if self
                    .blend_ids
                    .as_ref()
                    .is_some_and(|(index, _)| *index == next_index)
                {
                    self.blend_rows(&mut out.entities, alpha as f32);
                    out.blend = Some(ReplayBlend {
                        next_full_tick: next_tick,
                        alpha: alpha as f32,
                    });
                }
            }
        }
        self.append_beacons(&mut out.entities, nav_index);
        out
    }

    fn locate_full(&mut self, tick: u32) -> Option<usize> {
        let ReplaySim {
            file, full_cursor, ..
        } = self;
        let Some(file) = file.as_ref() else {
            *full_cursor = None;
            return None;
        };
        locate_sorted(
            file.fulls.len(),
            |index| file.fulls[index].tick,
            tick,
            full_cursor,
            note_full_cmp,
        )
    }

    fn locate_nav(&mut self, tick: u32) -> Option<usize> {
        let ReplaySim { file, nav_cursor, .. } = self;
        let Some(file) = file.as_ref() else {
            *nav_cursor = None;
            return None;
        };
        locate_sorted(
            file.navs.len(),
            |index| file.navs[index].tick,
            tick,
            nav_cursor,
            note_nav_cmp,
        )
    }

    fn decode_full(&mut self, index: usize) {
        if self.decoded.as_ref().is_some_and(|(i, _)| *i == index) {
            return;
        }
        let decoded = {
            let Some(file) = self.file.as_ref() else { return };
            let Some(full) = file.fulls.get(index) else { return };
            self.codec.decode(&full.snapshot)
        };
        self.decoded = Some((index, decoded));
    }

    fn full_tick_at(&self, index: usize) -> Option<u32> {
        self.file
            .as_ref()
            .and_then(|file| file.fulls.get(index).map(|full| full.tick))
    }

    fn nav_tick_at(&self, index: usize) -> Option<u32> {
        self.file
            .as_ref()
            .and_then(|file| file.navs.get(index).map(|nav| nav.tick))
    }

    fn decode_error_at(&self, index: usize) -> Option<String> {
        match self.decoded.as_ref().filter(|(i, _)| *i == index) {
            Some((_, Err(error))) => Some(error.clone()),
            _ => None,
        }
    }

    /// The next full a blend may move toward: its index, its tick, and the current full's tick.
    fn blend_target(&self, full_index: Option<usize>, tick: u32) -> Option<(usize, u32, u32)> {
        let index = full_index?;
        let file = self.file.as_ref()?;
        let full_tick = file.fulls.get(index)?.tick;
        let next = file.fulls.get(index + 1)?;
        (next.tick > tick && next.tick - full_tick <= self.blend_gap).then_some((
            index + 1,
            next.tick,
            full_tick,
        ))
    }

    fn ensure_entity_rows(&mut self, full_index: Option<usize>) {
        if self.entity_key == Some(full_index) {
            return;
        }
        self.entity_rows.clear();
        let decoded_rows = match self
            .decoded
            .as_ref()
            .filter(|(index, _)| full_index == Some(*index))
        {
            Some((_, Ok(rows))) => rows.clone(),
            _ => Vec::new(),
        };
        self.entity_rows.extend(decoded_rows);
        self.entity_key = Some(full_index);
    }

    /// Nav beacons after the blend, so a beacon is never moved toward the next full.
    fn append_beacons(&self, entities: &mut Vec<ReplayEntity>, nav_index: Option<usize>) {
        let Some(index) = nav_index else { return };
        let Some(nav) = self.file.as_ref().and_then(|file| file.navs.get(index)) else {
            return;
        };
        for beacon in &nav.beacons {
            entities.push(ReplayEntity {
                id: BEACON_ID_BASE | u64::from(beacon.id),
                kind: beacon_kind(),
                team: u32::from(beacon.team),
                translation: beacon.position(),
                rotation: Quat::IDENTITY,
            });
        }
    }

    fn ensure_blend_map(&mut self, next_index: usize) {
        if self
            .blend_ids
            .as_ref()
            .is_some_and(|(index, _)| *index == next_index)
        {
            return;
        }
        if self
            .decoded_next
            .as_ref()
            .is_none_or(|(index, _)| *index != next_index)
        {
            let decoded = {
                let Some(file) = self.file.as_ref() else { return };
                let Some(full) = file.fulls.get(next_index) else {
                    return;
                };
                self.codec.decode(&full.snapshot)
            };
            self.decoded_next = Some((next_index, decoded));
            // The map belongs to the decoded full it indexes: a failed decode of this full must
            // not leave an older full's map that later passes for a blend target.
            self.blend_ids = None;
        }
        let map = match self
            .decoded_next
            .as_ref()
            .filter(|(index, _)| *index == next_index)
        {
            Some((_, Ok(targets))) => {
                note_blend_map();
                targets
                    .iter()
                    .enumerate()
                    .map(|(index, entity)| (entity.id, index))
                    .collect()
            }
            _ => return,
        };
        self.blend_ids = Some((next_index, map));
    }

    fn blend_rows(&self, entities: &mut [ReplayEntity], alpha: f32) {
        let Some((_, map)) = &self.blend_ids else { return };
        let Some((_, Ok(targets))) = &self.decoded_next else {
            return;
        };
        let _span = tracing::trace_span!(target: "regia_framework", "replay_blend").entered();
        for entity in entities.iter_mut() {
            let Some(&index) = map.get(&entity.id) else {
                continue;
            };
            let Some(target) = targets.get(index) else {
                continue;
            };
            entity.translation = entity.translation.lerp(target.translation, alpha);
            entity.rotation = nlerp(entity.rotation, target.rotation, alpha);
        }
    }

    /// Verifies the loaded replay with this player's codec. `None` with nothing loaded.
    #[must_use]
    pub fn verify(&self) -> Option<VerifyReport> {
        self.file.as_ref().map(|f| f.verify(self.codec.as_ref()))
    }
}

#[cfg(test)]
pub(crate) mod replay_counts {
    use std::cell::Cell;

    thread_local! {
        static BLEND_MAPS: Cell<u32> = const { Cell::new(0) };
        static FULL_CMPS: Cell<u64> = const { Cell::new(0) };
        static NAV_CMPS: Cell<u64> = const { Cell::new(0) };
    }

    pub(crate) fn reset() {
        BLEND_MAPS.with(|c| c.set(0));
        FULL_CMPS.with(|c| c.set(0));
        NAV_CMPS.with(|c| c.set(0));
    }

    pub(crate) fn blend_maps() -> u32 {
        BLEND_MAPS.with(|c| c.get())
    }

    pub(crate) fn full_cmps() -> u64 {
        FULL_CMPS.with(|c| c.get())
    }

    pub(crate) fn add_blend_map() {
        BLEND_MAPS.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn add_full() {
        FULL_CMPS.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn add_nav() {
        NAV_CMPS.with(|c| c.set(c.get() + 1));
    }
}

fn note_full_cmp() {
    #[cfg(test)]
    replay_counts::add_full();
}

fn note_nav_cmp() {
    #[cfg(test)]
    replay_counts::add_nav();
}

fn note_blend_map() {
    #[cfg(test)]
    replay_counts::add_blend_map();
}

/// The shared `"beacon"` kind. One allocation for the process, cloned into each beacon row.
fn beacon_kind() -> Arc<str> {
    static KIND: OnceLock<Arc<str>> = OnceLock::new();
    Arc::clone(KIND.get_or_init(|| Arc::from("beacon")))
}

/// Index of the last record at or before `tick`, or `None` when every record is later.
///
/// The same tick returns the cached index and compares nothing. A later tick walks forward from
/// that index. A seek backward is a binary search. `note` runs once per comparison.
fn locate_sorted(
    len: usize,
    tick_of: impl Fn(usize) -> u32,
    tick: u32,
    cursor: &mut Option<(u32, usize)>,
    mut note: impl FnMut(),
) -> Option<usize> {
    if len == 0 {
        *cursor = None;
        return None;
    }
    if let Some((cached, index)) = *cursor
        && cached == tick
        && index < len
        && tick_of(index) <= tick
        && (index + 1 == len || tick_of(index + 1) > tick)
    {
        return Some(index);
    }
    let forward = match *cursor {
        Some((cached, index)) if tick > cached && index < len && tick_of(index) <= tick => Some(index),
        _ => None,
    };
    let found = if let Some(start) = forward {
        let mut index = start;
        while index + 1 < len {
            note();
            if tick_of(index + 1) <= tick {
                index += 1;
            } else {
                break;
            }
        }
        (tick_of(index) <= tick).then_some(index)
    } else {
        let mut lo = 0;
        let mut hi = len;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            note();
            if tick_of(mid) <= tick {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo.checked_sub(1)
    };
    *cursor = found.map(|index| (tick, index));
    found
}

/// Normalized linear blend of two rotations along the shorter arc. Only `+`, `*` and one square
/// root, so the result is the same on every platform.
fn nlerp(from: Quat, to: Quat, alpha: f32) -> Quat {
    let to = if from.dot(to) < 0.0 { -to } else { to };
    let blended = from * (1.0 - alpha) + to * alpha;
    let length = blended.length();
    if length > 0.0 { blended / length } else { from }
}

impl ExternalSim for ReplaySim {
    type Snapshot = ReplaySnapshot;
    type Event = ReplayEvent;

    /// A replay has no inputs to take; playback is the recording.
    fn submit_input(&mut self, _input: &BridgeInput) {}

    fn read_snapshot(&mut self, frame_time: Duration, out: &mut Self::Snapshot) -> SnapshotTick {
        self.advance_clock(frame_time);
        *out = self.presented_snapshot();
        SnapshotTick(u64::from(self.tick))
    }

    fn drain_events(&mut self, out: &mut Vec<Self::Event>) {
        out.append(&mut self.pending);
        self.last_step_events = 0;
    }

    fn event_kind(event: &Self::Event) -> String {
        match event {
            ReplayEvent::Moment(m) => format!("moment.{}", m.kind.name()),
            ReplayEvent::Command { .. } => "command".to_owned(),
            ReplayEvent::Abort(_) => "abort".to_owned(),
        }
    }
}

// ---------------------------------------------------------------------------
// Synthetic replay writer (tests, tooling)
// ---------------------------------------------------------------------------

/// Builds replay bytes by hand, record by record, following `spec/replay/do27-replay-v3.md`.
/// For tests and tooling that need a replay without any DO27 data; it can also write the
/// malformed shapes the reader must refuse (`raw`). Nothing is checked: counts are written as one
/// byte and string lengths as two, so more than 255 players or peers, more than 255 input rows or
/// a string over 65,535 bytes writes a file the reader refuses or misreads (audit v034 cook L2).
/// Keep within those widths for a valid replay.
#[derive(Debug, Clone, Default)]
pub struct SyntheticReplay {
    buf: Vec<u8>,
}

impl SyntheticReplay {
    /// Starts a file with `header` (version 2 or 3 as `header.version` says).
    #[must_use]
    pub fn new(header: &ReplayHeader) -> Self {
        let mut w = Self::default();
        w.buf.extend_from_slice(&REPLAY_MAGIC);
        w.u32(header.version);
        w.string(&header.protocol);
        w.u32(header.tick_rate);
        w.string(&header.lobby_preset);
        if header.version == 3 {
            w.buf.extend_from_slice(&header.seed.to_le_bytes());
            w.string(&header.bootstrap);
            w.string(&header.map);
            w.string(&header.mode);
            w.buf.push(header.teams);
            w.buf.push(header.players.len() as u8);
            for p in &header.players {
                w.buf.push(p.peer_id);
                w.string(&p.account_id);
                w.string(&p.callsign);
                w.buf.push(p.role);
                w.buf.push(p.team);
            }
            w.buf.extend_from_slice(&header.input_delay.to_le_bytes());
            w.buf.push(header.peers.len() as u8);
            for p in &header.peers {
                w.buf.push(p.peer_id);
                w.buf.push(p.role);
            }
            w.string(&header.sim_git_sha);
            w.u32(header.sim_abi);
            w.string(&header.content_hash);
            w.string(&header.config_hash);
        }
        w
    }

    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn string(&mut self, s: &str) {
        self.buf.extend_from_slice(&(s.len() as u16).to_le_bytes());
        self.buf.extend_from_slice(s.as_bytes());
    }

    /// Appends a `0x02` applied-inputs record.
    #[must_use]
    pub fn inputs(mut self, tick: u32, rows: &[(u8, PilotInput)]) -> Self {
        self.buf.push(TAG_INPUTS);
        self.u32(tick);
        self.buf.push(rows.len() as u8);
        for (peer, i) in rows {
            self.buf.push(*peer);
            self.buf.push(i.throttle as u8);
            self.buf.push(i.strafe as u8);
            self.buf.push(i.steer as u8);
            self.buf.extend_from_slice(&i.aim_pitch.to_le_bytes());
            self.buf.extend_from_slice(&i.action_bits.to_le_bytes());
            self.buf.push(i.stance);
        }
        self
    }

    /// Appends a version 3 `0x01` full with a stored state hash.
    #[must_use]
    pub fn full(mut self, snapshot: &[u8], state_hash: u64) -> Self {
        self.buf.push(TAG_FULL);
        self.u32(snapshot.len() as u32);
        self.buf.extend_from_slice(snapshot);
        self.buf.extend_from_slice(&state_hash.to_le_bytes());
        self
    }

    /// Appends a `0x03` salvage ledger.
    #[must_use]
    pub fn salvage(mut self, ledger: &SalvageLedger) -> Self {
        self.buf.push(TAG_SALVAGE);
        self.u32(ledger.tick);
        for row in &ledger.rows {
            for cell in row {
                self.u32(*cell);
            }
        }
        self
    }

    /// Appends a `0x04` nav record.
    #[must_use]
    pub fn nav(mut self, nav: &NavRecord) -> Self {
        let mut body = Vec::new();
        body.extend_from_slice(&(nav.beacons.len() as u16).to_le_bytes());
        for b in &nav.beacons {
            body.extend_from_slice(&b.id.to_le_bytes());
            body.extend_from_slice(&[b.owner_slot, b.team, b.number]);
            body.extend_from_slice(&b.x_cm.to_le_bytes());
            body.extend_from_slice(&b.y_cm.to_le_bytes());
            body.extend_from_slice(&b.z_cm.to_le_bytes());
            body.extend_from_slice(&b.health.to_le_bytes());
        }
        body.extend_from_slice(&(nav.waypoints.len() as u16).to_le_bytes());
        for w in &nav.waypoints {
            body.extend_from_slice(&[w.team, w.slot]);
            body.extend_from_slice(&w.id.to_le_bytes());
        }
        self.buf.push(TAG_NAV);
        self.u32(nav.tick);
        self.u32(body.len() as u32);
        self.buf.extend_from_slice(&body);
        self
    }

    /// Appends a `0x05` peer-commands record.
    #[must_use]
    pub fn commands(mut self, record: &PeerCommands) -> Self {
        self.buf.push(TAG_COMMANDS);
        self.u32(record.tick);
        self.buf.push(record.commands.len() as u8);
        for c in &record.commands {
            self.buf.push(c.peer_id);
            self.buf.push(u8::from(c.ok));
            self.buf.push(c.kind.len() as u8);
            self.buf.extend_from_slice(c.kind.as_bytes());
            self.buf.extend_from_slice(&(c.text.len() as u16).to_le_bytes());
            self.buf.extend_from_slice(c.text.as_bytes());
        }
        self
    }

    /// Appends a `0x06` Moments record for `tick`. Positions are written as given (the reader
    /// applies [`sim_to_regia`], the identity).
    #[must_use]
    pub fn moments(mut self, tick: u32, moments: &[Moment]) -> Self {
        self.buf.push(TAG_MOMENTS);
        self.u32(tick);
        self.buf.extend_from_slice(&(moments.len() as u16).to_le_bytes());
        for m in moments {
            let start = self.buf.len();
            self.buf.extend_from_slice(&m.tick.to_le_bytes());
            self.buf.push(m.kind.to_wire());
            self.u32(m.span_match);
            self.u32(m.span_round);
            self.u32(m.span_fight);
            for p in &m.participants {
                self.buf.push(p.role);
                self.u32(p.entity_id);
            }
            for c in [m.position.x, m.position.y, m.position.z] {
                self.buf.extend_from_slice(&c.to_le_bytes());
            }
            self.u32(m.damage);
            let weapon = m.weapon.as_bytes();
            let used = weapon.len().min(32);
            self.buf.push(used as u8);
            let mut field = [0u8; 32];
            field[..used].copy_from_slice(&weapon[..used]);
            self.buf.extend_from_slice(&field);
            self.u32(m.attacker);
            self.u32(m.victim);
            self.buf.push(m.importance);
            debug_assert_eq!(self.buf.len() - start, MOMENT_WIRE_BYTES);
        }
        self
    }

    /// Appends a `0x07` abort trailer.
    #[must_use]
    pub fn abort(mut self, tick: u32, reason: &str) -> Self {
        self.buf.push(TAG_ABORT);
        self.u32(tick);
        self.string(reason);
        self
    }

    /// Appends raw bytes (for malformed shapes).
    #[must_use]
    pub fn raw(mut self, bytes: &[u8]) -> Self {
        self.buf.extend_from_slice(bytes);
        self
    }

    /// The finished file bytes.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fnv1a(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
        })
    }

    /// Test snapshots are 3-byte entity rows: `id, team, x` (x an i8 in metres).
    struct TestCodec {
        hashes: bool,
    }

    impl FullCodec for TestCodec {
        fn decode(&self, snapshot: &[u8]) -> Result<Vec<ReplayEntity>, String> {
            if !snapshot.len().is_multiple_of(3) {
                return Err("snapshot length is not a multiple of 3".into());
            }
            Ok(snapshot
                .as_chunks::<3>()
                .0
                .iter()
                .map(|c| ReplayEntity {
                    id: u64::from(c[0]),
                    kind: "warhull".into(),
                    team: u32::from(c[1]),
                    translation: sim_to_regia(Vec3::new(f32::from(c[2] as i8), 0.0, 0.0)),
                    rotation: Quat::IDENTITY,
                })
                .collect())
        }

        fn state_hash(&self, snapshot: &[u8]) -> Option<u64> {
            self.hashes.then(|| fnv1a(snapshot))
        }
    }

    fn header() -> ReplayHeader {
        ReplayHeader {
            version: 3,
            protocol: "test/1".into(),
            tick_rate: 60,
            lobby_preset: "alpha".into(),
            seed: 0xDEAD_BEEF_0123,
            bootstrap: "bg_a".into(),
            map: "ironreach".into(),
            mode: "tdm".into(),
            teams: 2,
            players: vec![
                PlayerRow {
                    peer_id: 2,
                    account_id: String::new(),
                    callsign: "Nova".into(),
                    role: 1,
                    team: 0,
                },
                PlayerRow {
                    peer_id: 1,
                    account_id: "ab".into(),
                    callsign: "Ghost".into(),
                    role: 0,
                    team: 1,
                },
            ],
            input_delay: 3,
            peers: vec![PeerRow { peer_id: 2, role: 1 }, PeerRow { peer_id: 1, role: 0 }],
            sim_git_sha: "unknown".into(),
            sim_abi: 7,
            content_hash: "c0ffee".into(),
            config_hash: String::new(),
        }
    }

    fn input(throttle: i8) -> PilotInput {
        PilotInput {
            throttle,
            strafe: -1,
            steer: 2,
            aim_pitch: -300,
            action_bits: 0b101,
            stance: 1,
        }
    }

    fn moment(tick: u64, drained: u32, kind: MomentKind, attacker: u32, victim: u32, fight: u32) -> Moment {
        Moment {
            drained_tick: drained,
            tick,
            kind,
            span_match: 1,
            span_round: 1,
            span_fight: fight,
            participants: [
                Participant {
                    role: if attacker == 0 { 0 } else { 1 },
                    entity_id: attacker,
                },
                Participant {
                    role: 2,
                    entity_id: victim,
                },
            ],
            position: Vec3::new(1.5, 2.0, -3.25),
            damage: 40,
            weapon: "plasma_cannon".into(),
            attacker,
            victim,
            importance: 50,
        }
    }

    /// Fulls at ticks 0, 30, 60; moments at 12 (hit), 45 (kill) and 700 (hit, a later fight);
    /// a command at 20; nav at 0 and 60; ends at tick 700.
    fn fixture() -> Vec<u8> {
        let snap0 = [1u8, 0, 5, 2, 1, 0xFB]; // id1 team0 x=5; id2 team1 x=-5
        let snap30 = [1u8, 0, 6, 2, 1, 0xFA];
        let snap60 = [1u8, 0, 7];
        let mut w = SyntheticReplay::new(&header());
        for tick in 0..=60u32 {
            w = w.inputs(tick, &[(1, input(tick as i8)), (2, input(-(tick as i8)))]);
            match tick {
                0 => {
                    w = w.full(&snap0, fnv1a(&snap0)).salvage(&SalvageLedger {
                        tick,
                        rows: [[1, 2, 3, 4, 5]; 4],
                    });
                    w = w.nav(&NavRecord {
                        tick,
                        beacons: vec![NavBeacon {
                            id: 9,
                            owner_slot: 1,
                            team: 0,
                            number: 3,
                            x_cm: 1250,
                            y_cm: 0,
                            z_cm: -800,
                            health: 100,
                        }],
                        waypoints: vec![NavWaypoint {
                            team: 0,
                            slot: 1,
                            id: 9,
                        }],
                    });
                }
                12 => w = w.moments(12, &[moment(12, 12, MomentKind::Hit, 1, 2, 1)]),
                20 => {
                    w = w.commands(&PeerCommands {
                        tick,
                        commands: vec![PeerCommand {
                            peer_id: 1,
                            ok: true,
                            kind: "build".into(),
                            text: "factory at 3,4".into(),
                        }],
                    });
                }
                30 => w = w.full(&snap30, fnv1a(&snap30)),
                45 => w = w.moments(45, &[moment(45, 45, MomentKind::Kill, 1, 2, 1)]),
                60 => {
                    w = w.full(&snap60, fnv1a(&snap60));
                    w = w.nav(&NavRecord {
                        tick,
                        beacons: vec![],
                        waypoints: vec![],
                    });
                }
                _ => {}
            }
        }
        w = w.inputs(700, &[]);
        w = w.moments(700, &[moment(700, 700, MomentKind::Hit, 2, 1, 2)]);
        w.finish()
    }

    fn sim() -> ReplaySim {
        let mut sim = ReplaySim::with_codec(TestCodec { hashes: true });
        sim.load_bytes(&fixture()).expect("fixture parses");
        sim
    }

    // -- parser ---------------------------------------------------------------

    #[test]
    fn parses_the_header_and_tables_in_file_order() {
        let file = ReplayFile::parse(&fixture()).expect("parse");
        assert_eq!(file.header, header());
        let order: Vec<u8> = file.header.players.iter().map(|p| p.peer_id).collect();
        assert_eq!(order, vec![2, 1], "the player table is not sorted");
        assert_eq!(role_name(file.header.players[0].role), "player_host");
        assert_eq!(file.fulls.len(), 3);
        assert_eq!(file.navs.len(), 2);
        assert_eq!(file.commands.len(), 1);
        assert_eq!(file.moments.len(), 3);
        assert_eq!(file.moment_batches, 3);
        assert_eq!(file.salvage[0].rows[3], [1, 2, 3, 4, 5]);
        assert_eq!(file.end_tick(), 700);
        assert!((file.duration_secs() - 700.0 / 60.0).abs() < 1e-9);
    }

    #[test]
    fn full_ticks_come_from_the_preceding_inputs_record() {
        let file = ReplayFile::parse(&fixture()).expect("parse");
        let ticks: Vec<u32> = file.fulls.iter().map(|f| f.tick).collect();
        assert_eq!(ticks, vec![0, 30, 60]);
        assert_eq!(file.fulls[1].snapshot, vec![1, 0, 6, 2, 1, 0xFA]);
        assert_eq!(file.fulls[1].state_hash, Some(fnv1a(&[1, 0, 6, 2, 1, 0xFA])));
        let at = file.inputs_at(5).expect("inputs at 5");
        assert_eq!(at.inputs[0], (1, input(5)));
    }

    #[test]
    fn moment_fields_round_trip() {
        let file = ReplayFile::parse(&fixture()).expect("parse");
        let m = &file.moments[0];
        assert_eq!(m.kind, MomentKind::Hit);
        assert_eq!(m.tick, 12);
        assert_eq!(m.drained_tick, 12);
        assert_eq!(m.weapon, "plasma_cannon");
        assert_eq!(m.position, Vec3::new(1.5, 2.0, -3.25));
        assert_eq!((m.attacker, m.victim, m.damage, m.importance), (1, 2, 40, 50));
        assert_eq!(
            m.participants[1],
            Participant {
                role: 2,
                entity_id: 2
            }
        );
        assert_eq!(file.moments[2].span_fight, 2);
    }

    #[test]
    fn weapon_stem_is_cut_at_32_bytes() {
        let mut long = moment(1, 1, MomentKind::Hit, 1, 2, 1);
        long.weapon = "w".repeat(40);
        let bytes = SyntheticReplay::new(&header()).moments(1, &[long]).finish();
        let file = ReplayFile::parse(&bytes).expect("parse");
        assert_eq!(file.moments[0].weapon.len(), 32);
    }

    #[test]
    fn version_2_header_fills_v3_fields_with_defaults() {
        let h = ReplayHeader {
            version: 2,
            protocol: "old".into(),
            tick_rate: 60,
            lobby_preset: "alpha".into(),
            ..ReplayHeader::default()
        };
        let bytes = SyntheticReplay::new(&h).inputs(0, &[(1, input(1))]).finish();
        let file = ReplayFile::parse(&bytes).expect("v2 parses");
        assert_eq!(file.header, h);
        assert_eq!(file.inputs.len(), 1);
    }

    #[test]
    fn version_2_full_is_an_error_not_a_guess() {
        let h = ReplayHeader {
            version: 2,
            tick_rate: 60,
            ..ReplayHeader::default()
        };
        let bytes = SyntheticReplay::new(&h)
            .inputs(0, &[])
            .raw(&[TAG_FULL, 1, 2, 3])
            .finish();
        assert!(matches!(ReplayFile::parse(&bytes), Err(ReplayError::V2Full(_))));
    }

    #[test]
    fn rejects_bad_magic_and_versions() {
        assert_eq!(ReplayFile::parse(b"NOPE\x03\0\0\0"), Err(ReplayError::BadMagic));
        assert_eq!(
            ReplayFile::parse(b""),
            Err(ReplayError::Truncated { offset: 0, needed: 4 })
        );
        let mut h = header();
        h.version = 5;
        let bytes = SyntheticReplay::new(&h).finish();
        assert_eq!(ReplayFile::parse(&bytes), Err(ReplayError::UnsupportedVersion(5)));
        h.version = 1;
        let bytes = SyntheticReplay::new(&h).finish();
        assert_eq!(ReplayFile::parse(&bytes), Err(ReplayError::UnsupportedVersion(1)));
    }

    #[test]
    fn unknown_tag_is_an_error_and_not_skipped() {
        let bytes = SyntheticReplay::new(&header())
            .inputs(0, &[])
            .raw(&[0x42, 0, 0])
            .finish();
        assert!(matches!(
            ReplayFile::parse(&bytes),
            Err(ReplayError::UnknownTag { tag: 0x42, .. })
        ));
    }

    #[test]
    fn full_before_any_inputs_has_no_tick() {
        let bytes = SyntheticReplay::new(&header()).full(&[1, 2, 3], 0).finish();
        assert!(matches!(
            ReplayFile::parse(&bytes),
            Err(ReplayError::FullWithoutTick(_))
        ));
    }

    #[test]
    fn full_body_len_past_the_end_is_truncated_without_allocating() {
        let bytes = SyntheticReplay::new(&header())
            .inputs(0, &[])
            .raw(&[TAG_FULL])
            .raw(&u32::MAX.to_le_bytes())
            .finish();
        assert!(matches!(
            ReplayFile::parse(&bytes),
            Err(ReplayError::Truncated { .. })
        ));
    }

    #[test]
    fn every_truncation_of_a_valid_file_is_an_error_or_a_shorter_file() {
        let bytes = fixture();
        // An Ok cut landed exactly on a record boundary.
        let shorter_ok = (0..bytes.len())
            .filter(|&cut| ReplayFile::parse(&bytes[..cut]).is_ok())
            .count();
        assert!(shorter_ok > 0 && shorter_ok < bytes.len());
    }

    #[test]
    fn corrupted_bytes_never_panic() {
        let bytes = fixture();
        let mut seed = 0x1234_5678_9abc_def0u64;
        for _ in 0..400 {
            let mut copy = bytes.clone();
            for _ in 0..4 {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let at = (seed >> 33) as usize % copy.len();
                copy[at] = (seed >> 13) as u8;
            }
            let _ = ReplayFile::parse(&copy);
        }
    }

    #[test]
    fn bad_moment_weapon_length_and_utf8_are_errors() {
        let good = SyntheticReplay::new(&header())
            .moments(1, &[moment(1, 1, MomentKind::Hit, 1, 2, 1)])
            .finish();
        let weapon_len_at = good.len() - MOMENT_WIRE_BYTES + 47;
        let mut over = good.clone();
        over[weapon_len_at] = 33;
        assert!(matches!(
            ReplayFile::parse(&over),
            Err(ReplayError::BadMoment {
                reason: "weapon length above 32",
                ..
            })
        ));
        let mut invalid = good;
        invalid[weapon_len_at + 1] = 0xFF;
        assert!(matches!(
            ReplayFile::parse(&invalid),
            Err(ReplayError::BadMoment {
                reason: "weapon stem is not UTF-8",
                ..
            })
        ));
    }

    #[test]
    fn short_moment_body_is_truncated() {
        let good = SyntheticReplay::new(&header())
            .moments(1, &[moment(1, 1, MomentKind::Hit, 1, 2, 1)])
            .finish();
        assert!(matches!(
            ReplayFile::parse(&good[..good.len() - 1]),
            Err(ReplayError::Truncated { .. })
        ));
    }

    #[test]
    fn bad_header_string_is_an_error() {
        let mut bytes = SyntheticReplay::new(&header()).finish();
        // protocol string starts at byte 8 (u16 length), its text at 10.
        bytes[10] = 0xFF;
        assert!(matches!(
            ReplayFile::parse(&bytes),
            Err(ReplayError::BadString { offset: 8 })
        ));
    }

    #[test]
    fn nav_body_must_match_its_counts() {
        let nav = NavRecord {
            tick: 0,
            beacons: vec![NavBeacon::default()],
            waypoints: vec![],
        };
        let mut bytes = SyntheticReplay::new(&header()).nav(&nav).finish();
        // Grow the declared body length by one: a stray trailing byte inside the body.
        let len_at = bytes.len() - (2 + 19 + 2) - 4;
        bytes[len_at] += 1;
        bytes.push(0);
        assert!(matches!(
            ReplayFile::parse(&bytes),
            Err(ReplayError::BadNav {
                reason: "body has bytes past its waypoints",
                ..
            })
        ));
    }

    #[test]
    fn abort_trailer_is_read() {
        let bytes = SyntheticReplay::new(&header())
            .inputs(5, &[])
            .abort(5, "desync")
            .finish();
        let file = ReplayFile::parse(&bytes).expect("parse");
        assert_eq!(
            file.abort,
            Some(AbortRecord {
                tick: 5,
                reason: "desync".into()
            })
        );
    }

    #[test]
    fn peer_command_ok_is_any_nonzero() {
        let record = PeerCommands {
            tick: 3,
            commands: vec![PeerCommand {
                peer_id: 4,
                ok: false,
                kind: "order".into(),
                text: String::new(),
            }],
        };
        let mut bytes = SyntheticReplay::new(&header()).commands(&record).finish();
        let ok_at = bytes.len() - (1 + 5 + 2) - 1; // before kind_len(1) "order"(5) text_len(2)
        bytes[ok_at] = 7;
        let file = ReplayFile::parse(&bytes).expect("parse");
        assert!(file.commands[0].commands[0].ok);
    }

    // -- coordinates ----------------------------------------------------------

    #[test]
    fn coordinates_are_pinned_in_one_place() {
        assert_eq!(
            sim_to_regia(Vec3::new(10.0, 2.0, -4.0)),
            Vec3::new(10.0, 2.0, -4.0)
        );
        assert_eq!(nav_cm_to_regia(1250, 50, -800), Vec3::new(12.5, 0.5, -8.0));
    }

    // -- verification ---------------------------------------------------------

    #[test]
    fn verify_passes_with_a_hashing_codec() {
        let report = sim().verify().expect("loaded");
        assert!(report.is_ok(), "{report:?}");
        assert_eq!(report.hash_checked, 3);
        assert_eq!(report.hash_unchecked, 0);
        assert_eq!(report.moment_ticks, 3);
        assert!(!report.aborted);
    }

    #[test]
    fn verify_reports_a_flipped_bit_at_its_tick() {
        let mut file = ReplayFile::parse(&fixture()).expect("parse");
        file.fulls[1].snapshot[2] ^= 0x01;
        let report = file.verify(&TestCodec { hashes: true });
        assert_eq!(report.mismatches, vec![30]);
        assert!(!report.is_ok());
        let mut file = ReplayFile::parse(&fixture()).expect("parse");
        file.fulls[2].state_hash = file.fulls[2].state_hash.map(|h| h ^ 1);
        assert_eq!(file.verify(&TestCodec { hashes: true }).mismatches, vec![60]);
    }

    #[test]
    fn verify_without_a_hashing_codec_skips_instead_of_passing() {
        let file = ReplayFile::parse(&fixture()).expect("parse");
        let report = file.verify(&OpaqueCodec);
        assert!(report.is_ok());
        assert_eq!((report.hash_checked, report.hash_unchecked), (0, 3));
    }

    #[test]
    fn verify_names_structural_defects() {
        let h = header();
        let mut dup = h.clone();
        dup.players[1].peer_id = 2;
        let bytes = SyntheticReplay::new(&dup)
            .inputs(5, &[])
            .inputs(3, &[])
            .inputs(10, &[])
            .full(&[1, 0, 0], 0)
            .finish();
        let file = ReplayFile::parse(&bytes).expect("parse");
        let report = file.verify(&OpaqueCodec);
        let all = report.defects.join("; ");
        assert!(all.contains("decrease"), "{all}");
        assert!(all.contains("duplicate peer_id 2 in the player table"), "{all}");
        let none = SyntheticReplay::new(&h).finish();
        let report = ReplayFile::parse(&none).expect("parse").verify(&OpaqueCodec);
        assert!(report.defects.iter().any(|d| d == "no full record"));
    }

    // -- queries --------------------------------------------------------------

    #[test]
    fn moment_queries_filter() {
        let file = ReplayFile::parse(&fixture()).expect("parse");
        let all = file.moments_matching(&MomentQuery::default());
        assert_eq!(all, vec![0, 1, 2]);
        let kills = MomentQuery {
            kind: Some(MomentKind::Kill),
            ..MomentQuery::default()
        };
        assert_eq!(file.moments_matching(&kills), vec![1]);
        let window = MomentQuery {
            from_tick: Some(13),
            to_tick: Some(699),
            ..MomentQuery::default()
        };
        assert_eq!(file.moments_matching(&window), vec![1]);
        let by_entity = MomentQuery {
            entity: Some(1),
            ..MomentQuery::default()
        };
        assert_eq!(
            file.moments_matching(&by_entity),
            vec![0, 1, 2],
            "entity 1 is in all three"
        );
        let by_attacker = MomentQuery {
            attacker: Some(2),
            ..MomentQuery::default()
        };
        assert_eq!(file.moments_matching(&by_attacker), vec![2]);
        let fight2 = MomentQuery {
            fight: Some(2),
            weapon: Some("plasma_cannon".into()),
            ..MomentQuery::default()
        };
        assert_eq!(file.moments_matching(&fight2), vec![2]);
        let important = MomentQuery {
            min_importance: 51,
            ..MomentQuery::default()
        };
        assert!(file.moments_matching(&important).is_empty());
    }

    #[test]
    fn moment_kind_names_round_trip() {
        for kind in [MomentKind::Hit, MomentKind::Down, MomentKind::Kill] {
            assert_eq!(MomentKind::parse(kind.name()), Some(kind));
            assert_eq!(MomentKind::from_wire(kind.to_wire()), kind);
        }
        assert_eq!(MomentKind::parse("9"), Some(MomentKind::Other(9)));
        assert_eq!(MomentKind::parse("nonsense"), None);
    }

    // -- playback -------------------------------------------------------------

    #[test]
    fn seek_picks_the_last_full_at_or_before_the_tick() {
        let mut sim = sim();
        assert_eq!(sim.seek(29), 29);
        let snap = sim.snapshot();
        assert_eq!(snap.full_tick, Some(0));
        let xs: Vec<f32> = snap
            .entities
            .iter()
            .filter(|e| e.kind.as_ref() == "warhull")
            .map(|e| e.translation.x)
            .collect();
        assert_eq!(xs, vec![5.0, -5.0]);
        assert_eq!(sim.seek(30), 30);
        assert_eq!(sim.snapshot().full_tick, Some(30));
        assert_eq!(sim.seek(5_000), 700, "clamped to the end");
        let snap = sim.snapshot();
        assert_eq!(snap.full_tick, Some(60));
        assert_eq!(
            snap.entities.len(),
            1,
            "one warhull; the second nav record has no beacons"
        );
        assert_eq!(snap.nav_tick, Some(60));
    }

    #[test]
    fn beacons_become_entities_in_regia_metres() {
        let mut sim = sim();
        sim.seek(10);
        let snap = sim.snapshot();
        let beacon = snap
            .entities
            .iter()
            .find(|e| e.kind.as_ref() == "beacon")
            .expect("beacon");
        assert_eq!(beacon.translation, Vec3::new(12.5, 0.0, -8.0));
        assert_eq!(beacon.id, BEACON_ID_BASE | 9);
    }

    #[test]
    fn seek_is_deterministic_and_order_independent() {
        let mut a = sim();
        let mut b = sim();
        a.seek(45);
        let sa = a.snapshot();
        b.seek(600);
        b.seek(10);
        b.seek(45);
        assert_eq!(sa, b.snapshot());
    }

    #[test]
    fn step_delivers_each_event_exactly_once() {
        let mut sim = sim();
        let mut out = Vec::new();
        assert_eq!(sim.step(11), 11);
        sim.drain_events(&mut out);
        assert!(out.is_empty());
        sim.step(1); // crosses tick 12
        sim.drain_events(&mut out);
        assert!(matches!(&out[..], [ReplayEvent::Moment(m)] if m.kind == MomentKind::Hit));
        out.clear();
        sim.step(8); // crosses tick 20, the command
        sim.drain_events(&mut out);
        assert!(
            matches!(&out[..], [ReplayEvent::Command { tick: 20, command }] if command.kind == "build" && command.ok)
        );
        out.clear();
        sim.step(0);
        sim.drain_events(&mut out);
        assert!(out.is_empty(), "no double delivery");
        sim.step(1000);
        sim.drain_events(&mut out);
        assert_eq!(out.len(), 2, "the kill at 45 and the hit at 700");
        assert_eq!(sim.tick(), 700);
        assert_eq!(sim.step(5), 0, "already at the end");
    }

    #[test]
    fn seek_fires_nothing_and_does_not_replay_events() {
        let mut sim = sim();
        let mut out = Vec::new();
        sim.step(50);
        sim.seek(5);
        sim.drain_events(&mut out);
        assert!(out.is_empty(), "a seek discards queued events");
        sim.step(10); // 5 -> 15 crosses tick 12 again
        sim.drain_events(&mut out);
        assert_eq!(out.len(), 1);
        out.clear();
        sim.seek(100);
        sim.step(1);
        sim.drain_events(&mut out);
        assert!(out.is_empty(), "seeking over a Moment never fires it");
    }

    #[test]
    fn abort_fires_as_an_event() {
        let bytes = SyntheticReplay::new(&header())
            .inputs(0, &[])
            .inputs(4, &[])
            .abort(4, "desync")
            .finish();
        let mut sim = ReplaySim::new();
        sim.load_bytes(&bytes).expect("load");
        let mut out = Vec::new();
        sim.step(10);
        sim.drain_events(&mut out);
        assert!(matches!(&out[..], [ReplayEvent::Abort(a)] if a.reason == "desync"));
        assert_eq!(ReplaySim::event_kind(&out[0]), "abort");
    }

    #[test]
    fn tick_zero_events_are_delivered_on_the_first_step() {
        let bytes = SyntheticReplay::new(&header())
            .inputs(0, &[])
            .inputs(4, &[])
            .abort(0, "start")
            .finish();
        let mut sim = ReplaySim::new();
        sim.load_bytes(&bytes).expect("load");
        let mut out = Vec::new();
        sim.step(2);
        sim.drain_events(&mut out);
        assert!(matches!(&out[..], [ReplayEvent::Abort(a)] if a.reason == "start"));

        // A seek never delivers, and a second pass does not repeat the event.
        sim.seek(0);
        sim.step(2);
        out.clear();
        sim.drain_events(&mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn moment_at_tick_zero_is_delivered_on_the_first_step_not_after_a_seek() {
        let bytes = SyntheticReplay::new(&header())
            .inputs(0, &[])
            .inputs(4, &[])
            .moments(0, &[moment(0, 0, MomentKind::Hit, 1, 2, 1)])
            .finish();
        let mut sim = ReplaySim::new();
        sim.load_bytes(&bytes).expect("load");
        let mut out = Vec::new();
        sim.step(1);
        sim.drain_events(&mut out);
        assert!(matches!(&out[..], [ReplayEvent::Moment(m)] if m.drained_tick == 0));
        sim.seek(0);
        sim.step(1);
        out.clear();
        sim.drain_events(&mut out);
        assert!(
            out.is_empty(),
            "seeking to the event tick, then one step, does not deliver a tick-0 Moment"
        );
    }

    #[test]
    fn frame_clock_is_exact_and_frame_rate_independent() {
        let run = |frame: Duration, frames: u32| {
            let mut sim = sim();
            sim.set_playing(true);
            let mut snap = ReplaySnapshot::default();
            for _ in 0..frames {
                sim.read_snapshot(frame, &mut snap);
            }
            sim.tick()
        };
        // One second of wall time at 60 ticks/s, whatever the frame rate.
        assert_eq!(run(Duration::from_millis(10), 100), 60);
        assert_eq!(run(Duration::from_millis(40), 25), 60);
        // Ragged frames still never lose or invent time: 300 x 3.333 ms is exactly 999.9 ms.
        assert_eq!(run(Duration::from_micros(3333), 300), 59);
        let mut sim = sim();
        sim.set_playing(true);
        sim.set_speed_permille(2000);
        let mut snap = ReplaySnapshot::default();
        let t = sim.read_snapshot(Duration::from_secs(1), &mut snap);
        assert_eq!(t, SnapshotTick(120));
        sim.set_speed_permille(0);
        assert_eq!(
            sim.read_snapshot(Duration::from_secs(1), &mut snap),
            SnapshotTick(120)
        );
    }

    #[test]
    fn paused_clock_does_not_advance_and_end_stops_playback() {
        let mut sim = sim();
        let mut snap = ReplaySnapshot::default();
        sim.read_snapshot(Duration::from_secs(5), &mut snap);
        assert_eq!(sim.tick(), 0);
        sim.set_playing(true);
        sim.read_snapshot(Duration::from_secs(60), &mut snap);
        assert_eq!(sim.tick(), 700);
        assert!(!sim.playing(), "playback stops at the end");
    }

    #[test]
    fn codec_failure_is_reported_not_fatal() {
        let bytes = SyntheticReplay::new(&header())
            .inputs(0, &[])
            .full(&[1, 2], 0)
            .finish();
        let mut sim = ReplaySim::with_codec(TestCodec { hashes: true });
        sim.load_bytes(&bytes).expect("load");
        let snap = sim.snapshot();
        assert!(snap.decode_error.is_some());
        assert!(snap.entities.is_empty());
    }

    #[test]
    fn opaque_codec_reports_a_decode_error_not_an_empty_match() {
        let bytes = SyntheticReplay::new(&header())
            .inputs(0, &[])
            .full(&[1, 2], 0)
            .finish();
        let mut sim = ReplaySim::new();
        sim.load_bytes(&bytes).expect("load");
        let snap = sim.snapshot();
        assert_eq!(snap.full_tick, Some(0));
        assert!(snap.entities.is_empty());
        assert!(
            snap.decode_error
                .as_deref()
                .is_some_and(|e| e.starts_with("no_full_codec"))
        );
    }

    #[test]
    fn empty_player_is_safe() {
        let mut sim = ReplaySim::new();
        assert_eq!(sim.seek(10), 0);
        assert_eq!(sim.step(10), 0);
        assert_eq!(sim.snapshot(), ReplaySnapshot::default());
        assert!(sim.verify().is_none());
        let mut snap = ReplaySnapshot::default();
        assert_eq!(
            sim.read_snapshot(Duration::from_secs(1), &mut snap),
            SnapshotTick(0)
        );
    }

    #[test]
    fn snapshot_feeds_the_bridge_view() {
        let mut sim = sim();
        sim.seek(0);
        let snap = sim.snapshot();
        let mut seen = Vec::new();
        snap.for_each_entity(|e| seen.push((e.id, e.kind.to_owned(), e.team)));
        assert!(seen.contains(&(1, "warhull".to_owned(), 0)));
        assert!(seen.contains(&(BEACON_ID_BASE | 9, "beacon".to_owned(), 0)));
    }

    #[test]
    fn load_path_reports_missing_files_and_reads_real_ones() {
        let mut sim = ReplaySim::new();
        let dir = std::env::temp_dir().join(format!("regia-replay-sim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let missing = sim.load_path(&dir.join("nope.do27replay"));
        assert!(matches!(missing, Err(ReplayError::Io(_))));
        let path = dir.join("fixture.do27replay");
        std::fs::write(&path, fixture()).expect("write");
        sim.load_path(&path).expect("load");
        assert_eq!(sim.end_tick(), 700);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bridge_plays_a_replay_into_the_world() {
        use crate::sim_bridge::{SimBridgePlugin, SimId};
        use regia_scene::Transform;
        let mut replay = sim();
        replay.seek(30);
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SimBridgePlugin::<ReplaySim>::new(replay)));
        app.update();
        let world = app.world_mut();
        let mut query = world.query::<(&SimId, &Transform)>();
        let mut found: Vec<(u64, f32)> = query.iter(world).map(|(id, t)| (id.0, t.translation.x)).collect();
        found.sort_by_key(|(id, _)| *id);
        // The full at tick 30 (ids 1 and 2) and the nav beacon of tick 0.
        assert_eq!(found, vec![(1, 6.0), (2, -6.0), (BEACON_ID_BASE | 9, 12.5)]);
    }

    // ---- Malformed-input table tests (agent-tests lane) -------------------------------------

    /// A valid file's header only (no records).
    fn header_only() -> Vec<u8> {
        SyntheticReplay::new(&header()).finish()
    }

    /// A parse that succeeded must not have produced more data than the input could hold.
    fn assert_bounded(file: &ReplayFile, input_len: usize) {
        assert!(file.moments.len() * MOMENT_WIRE_BYTES <= input_len);
        assert!(file.inputs.len() <= input_len);
        assert!(file.fulls.iter().map(|f| f.snapshot.len()).sum::<usize>() <= input_len);
        assert!(file.navs.len() <= input_len);
    }

    #[test]
    fn empty_and_tiny_inputs_are_errors() {
        assert!(matches!(
            ReplayFile::parse(&[]),
            Err(ReplayError::Truncated { .. })
        ));
        assert!(matches!(
            ReplayFile::parse(b"DO"),
            Err(ReplayError::Truncated { .. })
        ));
        assert!(matches!(
            ReplayFile::parse(b"DO27"),
            Err(ReplayError::Truncated { .. })
        ));
        assert!(matches!(
            ReplayFile::parse(b"DO27\x03\x00\x00"),
            Err(ReplayError::Truncated { .. })
        ));
        assert!(matches!(
            ReplayFile::parse(b"XXXXXXXXXXXX"),
            Err(ReplayError::BadMagic)
        ));
    }

    #[test]
    fn every_prefix_of_the_header_is_an_error() {
        let bytes = header_only();
        for cut in 0..bytes.len() {
            assert!(
                ReplayFile::parse(&bytes[..cut]).is_err(),
                "a header cut at {cut} of {} parsed",
                bytes.len()
            );
        }
        assert!(ReplayFile::parse(&bytes).is_ok());
    }

    #[test]
    fn version_2_header_prefixes_are_errors_too() {
        let mut h = header();
        h.version = 2;
        let bytes = SyntheticReplay::new(&h).finish();
        for cut in 0..bytes.len() {
            assert!(ReplayFile::parse(&bytes[..cut]).is_err(), "v2 cut at {cut}");
        }
        assert!(ReplayFile::parse(&bytes).is_ok());
    }

    #[test]
    fn huge_header_string_prefix_is_truncated() {
        // protocol string length 0xFFFF with nothing behind it.
        let bytes = [b"DO27".as_slice(), &3u32.to_le_bytes(), &[0xFF, 0xFF]].concat();
        assert!(matches!(
            ReplayFile::parse(&bytes),
            Err(ReplayError::Truncated { .. })
        ));
    }

    #[test]
    fn player_and_peer_counts_past_the_data_are_errors() {
        let mut h = header();
        h.players.clear();
        h.peers.clear();
        let none = SyntheticReplay::new(&h).finish();
        assert!(ReplayFile::parse(&none).is_ok());
        // Bytes after the players count: input_delay u64, peers u8, then three strings and a u32.
        let after_players = 8 + 1 + (2 + h.sim_git_sha.len()) + 4 + (2 + h.content_hash.len()) + 2;
        let players_at = none.len() - after_players - 1;
        assert_eq!(none[players_at], 0);
        let mut lie = none.clone();
        lie[players_at] = 255;
        assert!(ReplayFile::parse(&lie).is_err());
        // And the peers count (the byte after input_delay).
        let peers_at = players_at + 1 + 8;
        assert_eq!(none[peers_at], 0);
        let mut lie = none;
        lie[peers_at] = 255;
        assert!(ReplayFile::parse(&lie).is_err());
    }

    #[test]
    fn huge_length_prefixes_in_every_record_are_errors_without_allocating() {
        fn cat(parts: &[&[u8]]) -> Vec<u8> {
            parts.concat()
        }
        let base = || SyntheticReplay::new(&header()).inputs(0, &[]);
        // (label, tail bytes)
        let max32 = u32::MAX.to_le_bytes();
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("full body_len u32::MAX", cat(&[&[TAG_FULL], &max32])),
            (
                "full body_len 0x7FFFFFFF",
                cat(&[&[TAG_FULL], &0x7FFF_FFFFu32.to_le_bytes()]),
            ),
            (
                "nav body_len u32::MAX",
                cat(&[&[TAG_NAV], &0u32.to_le_bytes(), &max32]),
            ),
            (
                "moments count 0xFFFF",
                cat(&[&[TAG_MOMENTS], &1u32.to_le_bytes(), &[0xFF, 0xFF]]),
            ),
            (
                "inputs count 255, no rows",
                cat(&[&[TAG_INPUTS], &1u32.to_le_bytes(), &[255]]),
            ),
            (
                "commands count 255, no rows",
                cat(&[&[TAG_COMMANDS], &1u32.to_le_bytes(), &[255]]),
            ),
            (
                "command kind_len 255",
                cat(&[&[TAG_COMMANDS], &1u32.to_le_bytes(), &[1, 1, 1, 255]]),
            ),
            (
                "command text_len 0xFFFF",
                cat(&[&[TAG_COMMANDS], &1u32.to_le_bytes(), &[1, 1, 1, 0, 0xFF, 0xFF]]),
            ),
            (
                "abort reason len 0xFFFF",
                cat(&[&[TAG_ABORT], &1u32.to_le_bytes(), &[0xFF, 0xFF]]),
            ),
        ];
        for (label, tail) in cases {
            let bytes = base().raw(&tail).finish();
            let result = ReplayFile::parse(&bytes);
            assert!(
                matches!(result, Err(ReplayError::Truncated { .. })),
                "{label}: {result:?}"
            );
        }
    }

    #[test]
    fn nav_counts_larger_than_the_body_are_bad_nav() {
        // Body of 4 bytes claiming 0xFFFF beacons.
        let mut body = vec![0xFF, 0xFF, 0, 0];
        let mut bytes = SyntheticReplay::new(&header()).finish();
        bytes.push(TAG_NAV);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.append(&mut body);
        assert!(matches!(
            ReplayFile::parse(&bytes),
            Err(ReplayError::BadNav { .. })
        ));

        // Zero beacons, 0xFFFF waypoints, nothing behind them.
        let body = [0u8, 0, 0xFF, 0xFF];
        let mut bytes = SyntheticReplay::new(&header()).finish();
        bytes.push(TAG_NAV);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&body);
        assert!(matches!(
            ReplayFile::parse(&bytes),
            Err(ReplayError::BadNav { .. })
        ));

        // Empty and 1-byte bodies.
        for body in [&[][..], &[0u8][..], &[0u8, 0][..]] {
            let mut bytes = SyntheticReplay::new(&header()).finish();
            bytes.push(TAG_NAV);
            bytes.extend_from_slice(&0u32.to_le_bytes());
            bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
            bytes.extend_from_slice(body);
            assert!(
                matches!(ReplayFile::parse(&bytes), Err(ReplayError::BadNav { .. })),
                "{body:?}"
            );
        }
    }

    #[test]
    fn every_bad_tag_byte_is_an_error() {
        for tag in (0u8..=255).filter(|t| !(1..=7).contains(t)) {
            let bytes = SyntheticReplay::new(&header())
                .raw(&[tag, 0, 0, 0, 0, 0])
                .finish();
            assert!(
                matches!(ReplayFile::parse(&bytes), Err(ReplayError::UnknownTag { tag: t, .. }) if t == tag),
                "tag {tag:#x}"
            );
        }
    }

    #[test]
    fn every_truncation_of_the_full_fixture_is_bounded_and_never_panics() {
        let bytes = fixture();
        let header_len = header_only().len();
        for cut in 0..bytes.len() {
            if let Ok(file) = ReplayFile::parse(&bytes[..cut]) {
                assert!(cut >= header_len, "a cut inside the header parsed ({cut})");
                assert_bounded(&file, cut);
            }
        }
    }

    #[test]
    fn every_single_byte_overwrite_never_panics_and_stays_bounded() {
        let bytes = fixture();
        for at in 0..bytes.len() {
            for value in [0x00u8, 0x01, 0x7F, 0x80, 0xFF] {
                let mut copy = bytes.clone();
                copy[at] = value;
                if let Ok(file) = ReplayFile::parse(&copy) {
                    assert_bounded(&file, copy.len());
                    // Everything downstream of a parse must also be total.
                    let _ = file.end_tick();
                    let _ = file.duration_secs();
                    let _ = file.verify(&TestCodec { hashes: true });
                }
            }
        }
    }

    #[test]
    fn random_garbage_after_a_valid_header_never_panics() {
        let head = header_only();
        let mut seed = 0xdead_beef_cafe_f00du64;
        for round in 0..300 {
            let mut copy = head.clone();
            let n = 1 + round % 200;
            for _ in 0..n {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                copy.push((seed >> 40) as u8);
            }
            if let Ok(file) = ReplayFile::parse(&copy) {
                assert_bounded(&file, copy.len());
            }
        }
    }

    #[test]
    fn parse_is_deterministic() {
        let bytes = fixture();
        let a = ReplayFile::parse(&bytes).expect("a");
        let b = ReplayFile::parse(&bytes).expect("b");
        assert_eq!(a.moments, b.moments);
        assert_eq!(a.fulls.len(), b.fulls.len());
        assert_eq!(a.end_tick(), b.end_tick());
    }

    #[test]
    fn oversized_file_is_refused_before_reading() {
        let dir = std::env::temp_dir().join(format!("regia-replay-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("big.do27replay");
        {
            let f = std::fs::File::create(&path).expect("create");
            // Sparse: no real disk or memory is used.
            f.set_len(MAX_REPLAY_BYTES + 1).expect("set_len");
        }
        assert!(matches!(
            ReplayFile::load(&path),
            Err(ReplayError::TooLarge { .. })
        ));
        // A directory is an I/O error, not a panic.
        assert!(matches!(ReplayFile::load(&dir), Err(ReplayError::Io(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn non_regular_files_are_refused() {
        // /dev/zero is endless; the loader must refuse it instead of reading it.
        assert!(matches!(
            ReplayFile::load(Path::new("/dev/zero")),
            Err(ReplayError::Io(_))
        ));
    }

    #[test]
    fn error_codes_are_stable_and_nonempty() {
        let errors = [
            ReplayError::Truncated { offset: 0, needed: 1 },
            ReplayError::BadMagic,
            ReplayError::UnsupportedVersion(9),
            ReplayError::BadString { offset: 0 },
            ReplayError::UnknownTag { tag: 9, offset: 0 },
            ReplayError::V2Full(0),
            ReplayError::FullWithoutTick(0),
            ReplayError::BadMoment {
                offset: 0,
                reason: "x",
            },
            ReplayError::BadNav {
                offset: 0,
                reason: "x",
            },
            ReplayError::TooLarge { size: 2, max: 1 },
            ReplayError::Io("x".into()),
        ];
        // The documented values (book/agents/replay.md); agent clients match on them.
        let documented = [
            "truncated",
            "bad_magic",
            "unsupported_version",
            "bad_string",
            "unknown_tag",
            "v2_full",
            "full_without_tick",
            "bad_moment",
            "bad_nav",
            "too_large",
            "io",
        ];
        let actual: Vec<&str> = errors.iter().map(ReplayError::code).collect();
        assert_eq!(actual, documented, "error codes are part of the agent protocol");
        let mut codes: Vec<&str> = actual;
        assert!(codes.iter().all(|c| {
            !c.is_empty()
                && c.chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
        }));
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), errors.len(), "codes are unique");
        assert!(errors.iter().all(|e| !e.to_string().is_empty()));
    }

    // ---- Version 2 fulls through a codec, the clock hold, sub-tick blending (ADR-1519) --------

    /// A codec for a made-up version 2 layout: a count byte, then that many 3-byte rows.
    struct CountedCodec;

    impl FullCodec for CountedCodec {
        fn decode(&self, snapshot: &[u8]) -> Result<Vec<ReplayEntity>, String> {
            TestCodec { hashes: false }.decode(snapshot.get(1..).unwrap_or_default())
        }

        fn state_hash(&self, _snapshot: &[u8]) -> Option<u64> {
            None
        }

        fn v2_full_len(&self, body: &[u8]) -> Option<usize> {
            body.first().map(|count| 1 + 3 * usize::from(*count))
        }
    }

    fn v2_header() -> ReplayHeader {
        ReplayHeader {
            version: 2,
            tick_rate: 60,
            ..ReplayHeader::default()
        }
    }

    #[test]
    fn a_version_2_full_is_read_with_a_codec_that_knows_where_it_ends() {
        let bytes = SyntheticReplay::new(&v2_header())
            .inputs(0, &[])
            .raw(&[TAG_FULL, 2, 1, 0, 5, 2, 1, 250])
            .inputs(3, &[(1, input(1))])
            .raw(&[TAG_FULL, 1, 1, 0, 7])
            .inputs(4, &[])
            .finish();
        // Without the writing build's codec the file is still refused, never guessed at.
        assert!(matches!(ReplayFile::parse(&bytes), Err(ReplayError::V2Full(_))));
        let file = ReplayFile::parse_with(&bytes, &CountedCodec).expect("v2 with a codec");
        assert_eq!(file.fulls.len(), 2);
        assert_eq!((file.fulls[0].tick, file.fulls[1].tick), (0, 3));
        assert_eq!(file.fulls[0].snapshot, [2, 1, 0, 5, 2, 1, 250]);
        assert!(
            file.fulls.iter().all(|f| f.state_hash.is_none()),
            "v2 stores no hash"
        );
        assert_eq!(file.inputs.len(), 3, "the records after each full still parse");
        // A version 2 full has no hash: verify counts it as unchecked, never passed.
        let report = file.verify(&CountedCodec);
        assert_eq!((report.hash_checked, report.hash_unchecked), (0, 2));

        let mut sim = ReplaySim::with_codec(CountedCodec);
        sim.load_bytes(&bytes)
            .expect("the player reads v2 fulls with its codec");
        sim.seek(3);
        let snap = sim.snapshot();
        assert_eq!(snap.full_tick, Some(3));
        assert_eq!(snap.entities.len(), 1);
        assert_eq!(snap.entities[0].translation.x, 7.0);
        // The default player keeps refusing it.
        assert!(matches!(
            ReplaySim::new().load_bytes(&bytes),
            Err(ReplayError::V2Full(_))
        ));
    }

    #[test]
    fn a_codec_length_past_the_end_or_a_v2_full_without_a_tick_is_an_error() {
        let past = SyntheticReplay::new(&v2_header())
            .inputs(0, &[])
            .raw(&[TAG_FULL, 9, 1, 0, 5])
            .finish();
        assert!(matches!(
            ReplayFile::parse_with(&past, &CountedCodec),
            Err(ReplayError::Truncated { .. })
        ));
        let untimed = SyntheticReplay::new(&v2_header()).raw(&[TAG_FULL, 0]).finish();
        assert!(matches!(
            ReplayFile::parse_with(&untimed, &CountedCodec),
            Err(ReplayError::FullWithoutTick(_))
        ));
        assert!(matches!(ReplayFile::parse(&untimed), Err(ReplayError::V2Full(_))));
        // Version 3 files parse the same whatever the codec.
        assert_eq!(
            ReplayFile::parse_with(&fixture(), &CountedCodec),
            ReplayFile::parse(&fixture())
        );
        assert!(ReplayError::V2Full(4).to_string().contains("version 3"));
    }

    #[test]
    fn a_held_clock_absorbs_the_next_frame_time_before_playing() {
        let mut sim = sim();
        sim.set_playing(true);
        sim.hold_clock(Duration::from_secs(1));
        let mut snap = ReplaySnapshot::default();
        sim.read_snapshot(Duration::from_millis(500), &mut snap);
        assert_eq!((sim.tick(), sim.held()), (0, Duration::from_millis(500)));
        sim.read_snapshot(Duration::from_secs(1), &mut snap);
        assert_eq!(sim.tick(), 30, "half of that frame was still held");
        assert_eq!(sim.held(), Duration::ZERO);
        // A paused player keeps its hold for when it plays.
        sim.set_playing(false);
        sim.hold_clock(Duration::from_secs(2));
        sim.read_snapshot(Duration::from_secs(5), &mut snap);
        assert_eq!((sim.tick(), sim.held()), (30, Duration::from_secs(2)));
    }

    /// One full per tick for ticks `0..=last`: entity 1 at `x = 4 · tick`, entity 2 only on even
    /// ticks (at x = -1).
    fn every_tick_fulls(last: u32) -> ReplaySim {
        let mut w = SyntheticReplay::new(&header());
        for tick in 0..=last {
            let mut rows = vec![1, 0, (4 * tick) as u8];
            if tick.is_multiple_of(2) {
                rows.extend_from_slice(&[2, 0, 255]);
            }
            w = w.inputs(tick, &[]).full(&rows, 0);
        }
        let mut sim = ReplaySim::with_codec(TestCodec { hashes: false });
        sim.load_bytes(&w.finish()).expect("load");
        sim
    }

    fn x_of(snap: &ReplaySnapshot, id: u64) -> Option<f32> {
        snap.entities.iter().find(|e| e.id == id).map(|e| e.translation.x)
    }

    #[test]
    fn slow_motion_blends_between_ticks_only_when_asked() {
        // 50 fps output at a quarter speed: 0.3 tick per frame.
        let frame = Duration::from_millis(20);
        let run = |gap: u32| {
            let mut sim = every_tick_fulls(10);
            sim.set_interpolation(gap);
            sim.set_playing(true);
            sim.set_speed_permille(250);
            let mut snap = ReplaySnapshot::default();
            (0..12)
                .map(|_| {
                    sim.read_snapshot(frame, &mut snap);
                    (sim.tick(), x_of(&snap, 1).unwrap_or(f32::NAN), snap.blend)
                })
                .collect::<Vec<_>>()
        };
        let stepped = run(0);
        let blended = run(1);
        for (k, ((tick_a, x_a, blend_a), (tick_b, x_b, blend_b))) in stepped.iter().zip(&blended).enumerate()
        {
            // Playback time after frame k, in tenths of a tick (integer, so no float floor).
            let tenths = 3 * (k as u32 + 1);
            let t = tenths as f32 / 10.0;
            assert_eq!(tick_a, tick_b, "blending never moves the playback tick");
            assert_eq!(*tick_a, tenths / 10, "frame {k}");
            assert_eq!(
                *x_a,
                4.0 * (tenths / 10) as f32,
                "without blending the pose holds for the tick"
            );
            assert!(blend_a.is_none());
            assert!((x_b - 4.0 * t).abs() < 1e-3, "frame {k}: {x_b} vs {}", 4.0 * t);
            let alpha = blend_b.map_or(0.0, |b| b.alpha);
            assert!((alpha - t.fract()).abs() < 1e-3, "frame {k}: alpha {alpha}");
        }
        // The same frame times give the same blended poses: no hidden state.
        assert_eq!(blended, run(1));
    }

    #[test]
    fn whole_tick_snapshots_are_the_same_with_blending_on() {
        let mut plain = every_tick_fulls(6);
        let mut blended = every_tick_fulls(6);
        blended.set_interpolation(1);
        for tick in [0, 1, 2, 5, 6] {
            plain.seek(tick);
            blended.seek(tick);
            assert_eq!(plain.snapshot(), blended.snapshot(), "tick {tick}");
            assert_eq!(plain.snapshot(), blended.presented_snapshot(), "tick {tick}");
        }
    }

    #[test]
    fn blending_never_changes_seek_step_or_the_hashes() {
        // Fulls every 4 ticks, blended across the gap: the presentation moves between them, the
        // exact snapshot, the tick, the events and the verification do not.
        let mut w = SyntheticReplay::new(&header());
        for tick in 0..=16u32 {
            w = w.inputs(tick, &[]);
            if tick.is_multiple_of(4) {
                w = w.full(&[1, 0, (4 * tick) as u8], 0);
            }
        }
        let bytes = w.finish();
        let load = |gap: u32| {
            let mut sim = ReplaySim::with_codec(TestCodec { hashes: false });
            sim.load_bytes(&bytes).expect("load");
            sim.set_interpolation(gap);
            sim
        };
        let (mut plain, mut blended) = (load(0), load(4));
        for tick in [3, 0, 7, 16, 9, 2, 12] {
            assert_eq!(plain.seek(tick), blended.seek(tick));
            let exact = plain.snapshot();
            assert_eq!(exact, blended.snapshot(), "seek {tick}");
            assert!(exact.blend.is_none());
            assert_eq!(
                exact,
                plain.presented_snapshot(),
                "blending off presents the exact pose"
            );
            let shown = blended.presented_snapshot();
            assert_eq!(shown.tick, exact.tick);
            if tick.is_multiple_of(4) {
                assert_eq!(shown, exact, "a tick on a full shows the full");
            } else {
                assert!(shown.blend.is_some(), "tick {tick} sits between fulls");
                assert_eq!(x_of(&shown, 1), Some(4.0 * tick as f32));
            }
        }
        plain.seek(1);
        blended.seek(1);
        assert_eq!(plain.step(6), blended.step(6));
        assert_eq!(plain.snapshot(), blended.snapshot());
        assert_eq!(plain.last_step_events(), blended.last_step_events());
        assert_eq!(plain.verify(), blended.verify());
    }

    #[test]
    fn a_corrupt_next_full_never_reports_a_blend_it_did_not_apply() {
        // Fulls at ticks 0, 4 and 8; the tick-8 full is corrupt (length not a multiple of 3).
        let mut w = SyntheticReplay::new(&header());
        for tick in 0..=8u32 {
            w = w.inputs(tick, &[]);
            match tick {
                0 => w = w.full(&[1, 0, 0], 0),
                4 => w = w.full(&[1, 0, 16], 0),
                8 => w = w.full(&[1, 0], 0),
                _ => {}
            }
        }
        let bytes = w.finish();
        let mut sim = ReplaySim::with_codec(TestCodec { hashes: false });
        sim.load_bytes(&bytes).expect("load");
        sim.set_interpolation(8);
        sim.seek(1);
        let first = sim.presented_snapshot();
        assert!(first.blend.is_some());
        sim.seek(5);
        let corrupt = sim.presented_snapshot();
        assert!(corrupt.blend.is_none(), "the tick-8 full does not decode");
        sim.seek(1);
        let again = sim.presented_snapshot();
        assert_eq!(again, first, "back between the good fulls, the blend is the same");
        assert!(
            x_of(&again, 1).is_some_and(|x| (x - 4.0).abs() < 1e-5),
            "a quarter of the way from 0 to 16: {again:?}"
        );
    }

    #[test]
    fn blending_spans_only_gaps_up_to_the_limit_and_keeps_unmatched_entities() {
        // Fulls at ticks 0 (entities 1 and 3) and 3 (entities 1 and 2).
        let bytes = SyntheticReplay::new(&header())
            .inputs(0, &[])
            .full(&[1, 0, 0, 3, 0, 9], 0)
            .inputs(1, &[])
            .inputs(2, &[])
            .inputs(3, &[])
            .full(&[1, 0, 6, 2, 0, 100], 0)
            .finish();
        let mut sim = ReplaySim::with_codec(TestCodec { hashes: false });
        sim.load_bytes(&bytes).expect("load");
        sim.seek(1);
        sim.set_interpolation(1);
        let near = sim.presented_snapshot();
        assert_eq!(
            (x_of(&near, 1), near.blend),
            (Some(0.0), None),
            "a 3-tick gap is over 1"
        );
        sim.set_interpolation(3);
        assert_eq!(sim.snapshot(), near, "the exact snapshot never blends");
        let far = sim.presented_snapshot();
        assert!(
            x_of(&far, 1).is_some_and(|x| (x - 2.0).abs() < 1e-5),
            "a third of the way from 0 to 6"
        );
        assert_eq!(far.blend.map(|b| b.next_full_tick), Some(3));
        assert_eq!(
            x_of(&far, 3),
            Some(9.0),
            "an entity gone in the next full keeps its pose"
        );
        assert_eq!(
            x_of(&far, 2),
            None,
            "an entity of the next full appears when it is reached"
        );
        sim.set_interpolation(u32::MAX);
        assert_eq!(sim.interpolation(), MAX_BLEND_GAP_TICKS);
    }

    #[test]
    fn rotations_blend_along_the_shorter_arc() {
        let q = Quat::from_rotation_y(0.4);
        let same = nlerp(q, -q, 0.5);
        assert!(same.dot(q).abs() > 0.999_999, "q and -q are the same rotation");
        let half = nlerp(
            Quat::IDENTITY,
            Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
            0.5,
        );
        assert!(half.angle_between(Quat::from_rotation_y(std::f32::consts::FRAC_PI_4)) < 1e-5);
        assert!(nlerp(q, Quat::IDENTITY, 0.0).angle_between(q) < 1e-5);
    }

    /// Two reads of the same tick share the decoded kind allocation, and a blend between the
    /// same two fulls builds the id map once.
    #[test]
    fn presented_snapshot_shares_kinds_across_frames() {
        let bytes = SyntheticReplay::new(&header())
            .inputs(0, &[])
            .full(&[1, 0, 4], 0)
            .inputs(1, &[])
            .full(&[1, 0, 8], 0)
            .finish();
        let mut sim = ReplaySim::with_codec(TestCodec { hashes: false });
        sim.load_bytes(&bytes).expect("load");
        let mut a = ReplaySnapshot::default();
        let mut b = ReplaySnapshot::default();
        sim.read_snapshot(Duration::ZERO, &mut a);
        sim.read_snapshot(Duration::ZERO, &mut b);
        let cached = match &sim.decoded {
            Some((_, Ok(rows))) => rows[0].kind.as_ptr(),
            other => panic!("decoded full missing: {other:?}"),
        };
        let mut problems = Vec::new();
        if a.entities[0].kind.as_ptr() != cached {
            problems.push(format!(
                "first snapshot kind {:p} is a copy of the decoded kind {cached:p}",
                a.entities[0].kind.as_ptr()
            ));
        }
        if b.entities[0].kind.as_ptr() != cached {
            problems.push(format!(
                "second snapshot kind {:p} is a copy of the decoded kind {cached:p}",
                b.entities[0].kind.as_ptr()
            ));
        }
        sim.set_interpolation(1);
        sim.set_playing(true);
        replay_counts::reset();
        sim.read_snapshot(Duration::from_millis(1), &mut a);
        let built = replay_counts::blend_maps();
        if built != 1 {
            problems.push(format!("first blended frame built {built} id maps, want 1"));
        }
        sim.read_snapshot(Duration::from_millis(1), &mut b);
        let again = replay_counts::blend_maps();
        if again != built {
            problems.push(format!(
                "second frame between the same fulls rebuilt the id map ({built} then {again})"
            ));
        }
        assert!(problems.is_empty(), "{}", problems.join("; "));
    }

    /// A second snapshot at the same early tick does not walk the later fulls, and a backward
    /// seek is a binary search.
    #[test]
    fn early_playback_does_not_rescan_later_fulls() {
        const N: u32 = 4096;
        let mut sim = every_tick_fulls(N - 1);
        let expect = |sim: &ReplaySim, tick: u32| {
            sim.file()
                .and_then(|f| f.fulls.iter().rev().find(|full| full.tick <= tick))
                .map(|full| full.tick)
        };
        sim.seek(0);
        replay_counts::reset();
        let first = sim.snapshot();
        let after_first = replay_counts::full_cmps();
        let second = sim.snapshot();
        let second_cmps = replay_counts::full_cmps() - after_first;
        assert_eq!(first.full_tick, expect(&sim, 0));
        assert_eq!(second.full_tick, expect(&sim, 0));
        assert!(
            second_cmps <= 1,
            "second snapshot at tick 0 compared {second_cmps} fulls (first walk {after_first})"
        );
        sim.seek(N - 1);
        let end = sim.snapshot();
        assert_eq!(end.full_tick, expect(&sim, N - 1));
        replay_counts::reset();
        sim.seek(0);
        let back = sim.snapshot();
        let back_cmps = replay_counts::full_cmps();
        let bound = u64::from(N.ilog2()) + 2;
        assert_eq!(back.full_tick, expect(&sim, 0));
        assert!(
            back_cmps <= bound,
            "backward seek compared {back_cmps} fulls, bound {bound}"
        );
    }
}
