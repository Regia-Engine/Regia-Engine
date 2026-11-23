// SPDX-License-Identifier: MPL-2.0
//! The desync bundle, `*.desync.ron` version 1 (spec 120 § 7.6 and § 12.4, a DO27 design; ADR-8455).
//!
//! A bundle holds what an offline replay needs to step a diverged lockstep match again: the
//! last agreed snapshot (opaque game bytes), every frame after it (each seat's input bytes, in
//! seat order), the subsystem rows each probed simulation reported, and the live verdict. The
//! host writes one when its probe names a subsystem ([`DesyncBundle::from_host`]); a diverged
//! seat can write its own before it takes the majority's state ([`DesyncBundle::from_seat`]).
//! `regia net desync-replay <bundle>` replays it through a registered
//! [`DesyncReplay`](super::replay::DesyncReplay) hook (the [`replay`](mod@super::replay) module).
//!
//! Decoding is for untrusted files: [`DesyncBundle::from_ron`] refuses text over
//! [`MAX_BUNDLE_BYTES`] before parsing, never sizes a buffer from a field of the file (every
//! list grows from what it actually reads), and checks every count and length against the
//! `MAX_*` limits here before anyone steps it. Format: `docs/formats/desync-ron.md`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use super::{DesyncReport, MAX_SUBSYSTEM_NAME_BYTES, MAX_SUBSYSTEMS, ProbeRows, SubsystemRow};
use crate::lockstep::{
    HostRole, LockstepFrame, LockstepHost, LockstepSeat, MembershipKind, SeatInput, Voter,
};
use crate::payload::{NetCodec, NetGame, protocol_id};

/// The `format_version` this build writes and reads.
pub const DESYNC_BUNDLE_FORMAT_VERSION: u32 = 1;
/// The file name suffix of a desync bundle.
pub const DESYNC_BUNDLE_SUFFIX: &str = ".desync.ron";
/// Largest bundle text read, bytes (refused before parsing).
pub const MAX_BUNDLE_BYTES: usize = 64 << 20;
/// Most frames a bundle holds.
pub const MAX_BUNDLE_FRAMES: usize = 8_192;
/// Most seats one frame lists.
pub const MAX_BUNDLE_SEATS: usize = 1_024;
/// Longest input of one seat for one tick, bytes (the lockstep `max_input_bytes` ceiling).
pub const MAX_BUNDLE_INPUT_BYTES: usize = 60_000;
/// Largest snapshot, bytes.
pub const MAX_SNAPSHOT_BYTES: usize = 32 << 20;
/// Most simulations with rows in a bundle.
pub const MAX_BUNDLE_PEERS: usize = 64;
/// Most rows one simulation contributes.
pub const MAX_BUNDLE_ROWS: usize = MAX_BUNDLE_FRAMES + 1;
/// Longest game id, bytes.
pub const MAX_GAME_ID_BYTES: usize = 128;
/// Longest free-text field (engine version, a refusal reason), bytes.
pub const MAX_TEXT_BYTES: usize = 256;

/// Bytes written as a lowercase hex string in the file (readable, diffable, and bounded by the
/// text it was read from).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HexBytes(pub Vec<u8>);

impl Serialize for HexBytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut text = String::with_capacity(self.0.len() * 2);
        for byte in &self.0 {
            text.push(char::from(DIGITS[usize::from(byte >> 4)]));
            text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
        }
        serializer.serialize_str(&text)
    }
}

impl<'de> Deserialize<'de> for HexBytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = HexBytes;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a string of hex digit pairs")
            }
            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<HexBytes, E> {
                decode_hex(text).map(HexBytes).map_err(E::custom)
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

fn decode_hex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err(format!("{} hex digits: not whole bytes", text.len()));
    }
    if text.len() / 2 > MAX_SNAPSHOT_BYTES {
        return Err(format!(
            "{} bytes of hex: more than {MAX_SNAPSHOT_BYTES}",
            text.len() / 2
        ));
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    // Sized from the text actually read, never from a count in the file.
    let mut out = Vec::with_capacity(text.len() / 2);
    let (pairs, _) = text.as_bytes().as_chunks::<2>();
    for [hi, lo] in pairs {
        match (digit(*hi), digit(*lo)) {
            (Some(hi), Some(lo)) => out.push((hi << 4) | lo),
            _ => return Err("a character that is not a hex digit".to_owned()),
        }
    }
    Ok(out)
}

/// Which game and build wrote a bundle: a replay hook must match it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleGame {
    /// `NetGame::GAME_ID`.
    pub game_id: String,
    /// `NetGame::PROTOCOL_VERSION`, the game's version for the wire.
    pub protocol_version: u32,
    /// [`protocol_id`] of the game: its id, protocol version, content hash and the wire version.
    pub protocol_id: u64,
    /// The `regia_net` version that wrote the bundle (information only).
    pub engine_version: String,
}

impl BundleGame {
    /// The identity of game `G` in this build.
    pub fn of<G: NetGame>() -> Self {
        Self {
            game_id: G::GAME_ID.to_owned(),
            protocol_version: G::PROTOCOL_VERSION,
            protocol_id: protocol_id::<G>(),
            engine_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

/// Who wrote a bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleWriter {
    /// The match's host (its probe rows and the live verdict are in the bundle).
    Host,
    /// A diverged seat, before it took the majority's state (only its own rows).
    Seat(u16),
}

/// The state the replay starts from.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BundleSnapshot {
    /// The tick the state was taken after; `None`: the match start (the first frame is tick 0).
    pub tick: Option<u64>,
    /// The game's encoding of its state (empty for the match start when the game needs none).
    pub data: HexBytes,
}

impl BundleSnapshot {
    /// The state after `tick`, encoded by the game.
    pub fn at(tick: u64, data: Vec<u8>) -> Self {
        Self {
            tick: Some(tick),
            data: HexBytes(data),
        }
    }

    /// The match start: the replay hook loads its initial state.
    pub fn start() -> Self {
        Self::default()
    }

    /// The first frame the replay steps.
    pub fn first_tick(&self) -> u64 {
        self.tick.map_or(0, |t| t.saturating_add(1))
    }
}

/// One seat's input for one tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleInput {
    /// The seat.
    pub seat: u16,
    /// The encoded bytes of the input the tick was stepped with (the seat's own, or the one the
    /// match's missing-input policy held for it); `None`: absent this tick.
    pub input: Option<HexBytes>,
}

/// A seat that joins or leaves at a frame's tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleMembership {
    /// The seat.
    pub seat: u16,
    /// `true`: joins; `false`: leaves.
    pub joins: bool,
}

/// One frame as the match stepped it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleFrame {
    /// The tick.
    pub tick: u64,
    /// Every listed seat's input, ascending by seat (decoding sorts them).
    pub inputs: Vec<BundleInput>,
    /// Seats that join or leave at this tick.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub membership: Vec<BundleMembership>,
}

impl BundleFrame {
    /// The input bytes of `seat` this tick (`None` when absent or not listed).
    pub fn input(&self, seat: u16) -> Option<&[u8]> {
        self.inputs
            .iter()
            .find(|i| i.seat == seat)
            .and_then(|i| i.input.as_ref())
            .map(|b| b.0.as_slice())
    }

    /// The frame with each present input decoded as `I` (the game's input type), in seat order.
    pub fn decode<I: NetCodec>(&self) -> Result<Vec<(u16, Option<I>)>, BundleError> {
        self.inputs
            .iter()
            .map(|i| {
                let value = match &i.input {
                    Some(bytes) => Some(I::from_bytes(&bytes.0).map_err(|e| {
                        BundleError::Invalid(format!(
                            "tick {} seat {}: input does not decode: {e}",
                            self.tick, i.seat
                        ))
                    })?),
                    None => None,
                };
                Ok((i.seat, value))
            })
            .collect()
    }
}

/// One simulation's subsystem rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundlePeer {
    /// Whose simulation.
    pub voter: Voter,
    /// Its rows (names and rows ascending by tick).
    pub rows: ProbeRows,
}

/// A desync bundle (`*.desync.ron`, format version 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename = "DesyncBundle")]
pub struct DesyncBundle {
    /// [`DESYNC_BUNDLE_FORMAT_VERSION`].
    pub format_version: u32,
    /// The game and build that wrote it.
    pub game: BundleGame,
    /// Who wrote it.
    pub writer: BundleWriter,
    /// The checkpoint of the verdict (a seat's bundle: its newest stepped tick).
    pub checkpoint: u64,
    /// The last agreed state.
    pub snapshot: BundleSnapshot,
    /// Every frame after the snapshot, in tick order, without a gap.
    pub frames: Vec<BundleFrame>,
    /// The subsystem rows of each probed simulation, ascending by voter.
    pub peers: Vec<BundlePeer>,
    /// The live probe's report, when the host had one.
    #[serde(default)]
    pub verdict: Option<DesyncReport>,
}

/// Why a bundle could not be read, written or replayed. [`BundleError::code`] is the agent and
/// CLI error code.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BundleError {
    /// The text is larger than [`MAX_BUNDLE_BYTES`].
    #[error("the bundle is {0} bytes, more than the {MAX_BUNDLE_BYTES} read")]
    TooLarge(usize),
    /// The text is not a bundle.
    #[error("not a desync bundle: {0}")]
    Parse(String),
    /// Another format version.
    #[error(
        "the bundle is format_version {found}; this build reads version {DESYNC_BUNDLE_FORMAT_VERSION} (replay it with the engine version that wrote it)"
    )]
    Version {
        /// The version in the file.
        found: u32,
    },
    /// A count, length or order is out of range.
    #[error("invalid desync bundle: {0}")]
    Invalid(String),
    /// The bundle was written by another game than the hook replays.
    #[error("the bundle was written by game `{bundle}`; the replay hook is for game `{hook}`")]
    GameMismatch {
        /// The bundle's game id.
        bundle: String,
        /// The hook's game id.
        hook: String,
    },
    /// The bundle was written by another version of the game.
    #[error(
        "the bundle was written by {game} protocol version {bundle_version} (protocol id {bundle_id:#018x}, engine {engine}); this replay hook is protocol version {hook_version} (protocol id {hook_id:#018x}). A different game version steps differently: replay it with the build that wrote it"
    )]
    GameVersion {
        /// The game id.
        game: String,
        /// The bundle's protocol version.
        bundle_version: u32,
        /// The bundle's protocol id.
        bundle_id: u64,
        /// The engine version that wrote it.
        engine: String,
        /// The hook's protocol version.
        hook_version: u32,
        /// The hook's protocol id.
        hook_id: u64,
    },
    /// No replay hook is registered for the bundle's game.
    #[error("no DesyncReplay hook is registered for game `{game}` (registered: {registered})")]
    NoHook {
        /// The bundle's game id.
        game: String,
        /// The registered game ids, comma separated (`none` when empty).
        registered: String,
    },
    /// The hook refused to load the snapshot or step a frame.
    #[error("the replay hook failed at tick {tick}: {reason}")]
    Replay {
        /// The tick it failed at (the snapshot tick for a load).
        tick: u64,
        /// The hook's reason.
        reason: String,
    },
    /// Reading or writing the file failed.
    #[error("{0}")]
    Io(String),
}

impl BundleError {
    /// The error code (lowercase ascii, digits and `_`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::TooLarge(_) => "desync_bundle_too_large",
            Self::Parse(_) => "desync_bundle_parse",
            Self::Version { .. } => "desync_bundle_version",
            Self::Invalid(_) => "desync_bundle_invalid",
            Self::GameMismatch { .. } => "desync_bundle_game_mismatch",
            Self::GameVersion { .. } => "desync_bundle_game_version",
            Self::NoHook { .. } => "desync_replay_no_hook",
            Self::Replay { .. } => "desync_replay_failed",
            Self::Io(_) => "desync_bundle_io",
        }
    }
}

/// Only the version, read first so a future version is named instead of failing on its fields.
#[derive(Deserialize)]
#[serde(rename = "DesyncBundle")]
struct Head {
    format_version: u32,
}

fn voter_of(role: HostRole) -> Option<Voter> {
    match role {
        HostRole::Listen(seat) => Some(Voter::Seat(seat)),
        HostRole::Observer => Some(Voter::Observer),
        HostRole::Relay => None,
    }
}

fn frame_of<I: NetCodec>(frame: &LockstepFrame<I>) -> Result<BundleFrame, BundleError> {
    let inputs = frame
        .inputs
        .iter()
        .map(|(seat, input)| {
            let input = match input {
                // A held input (the missing-input policy's) is what the live step used: the replay
                // must step with the same value.
                SeatInput::Present(value) | SeatInput::Held(value) => {
                    Some(HexBytes(value.to_bytes().map_err(|e| {
                        BundleError::Invalid(format!(
                            "tick {} {seat}: input does not encode: {e}",
                            frame.tick
                        ))
                    })?))
                }
                SeatInput::Absent => None,
            };
            Ok(BundleInput { seat: seat.0, input })
        })
        .collect::<Result<Vec<_>, BundleError>>()?;
    let membership = frame
        .membership
        .iter()
        .map(|m| BundleMembership {
            seat: m.seat.0,
            joins: m.kind == MembershipKind::Join,
        })
        .collect();
    Ok(BundleFrame {
        tick: frame.tick,
        inputs,
        membership,
    })
}

fn frames_of<'a, I: NetCodec + 'a>(
    frames: impl Iterator<Item = &'a LockstepFrame<I>>,
) -> Result<Vec<BundleFrame>, BundleError> {
    frames.take(MAX_BUNDLE_FRAMES).map(frame_of).collect()
}

/// Adds `extra`'s rows that `rows` lacks (same names only), keeping the ticks ascending.
fn merge_rows(rows: &mut ProbeRows, extra: ProbeRows) {
    if rows.names != extra.names {
        return;
    }
    let held: BTreeSet<u64> = rows.rows.iter().map(|r| r.tick).collect();
    rows.rows
        .extend(extra.rows.into_iter().filter(|r| !held.contains(&r.tick)));
    rows.rows.sort_by_key(|r| r.tick);
    rows.rows.truncate(MAX_BUNDLE_ROWS);
}

impl DesyncBundle {
    /// The host's bundle: `snapshot` (the game's state at the last agreed checkpoint, or the
    /// match start), every frame after it the host still holds, the rows of the last probe
    /// (the diverged voters and the reference, ascending by voter) plus the host's own rows since
    /// the snapshot, and the newest probe report.
    ///
    /// Inputs are written as `G::Input::to_bytes` of the decoded value: a replay decodes them to
    /// the same values the live step used.
    pub fn from_host<G: NetGame>(
        host: &LockstepHost<G>,
        snapshot: BundleSnapshot,
    ) -> Result<Self, BundleError> {
        let first = snapshot.first_tick();
        let frames = frames_of(host.frames_since(first))?;
        let mut peers: Vec<BundlePeer> = host
            .desync_probe_rows()
            .into_iter()
            .map(|(voter, rows)| BundlePeer { voter, rows })
            .collect();
        if let Some(own) = voter_of(host.role()) {
            let window = host.subsystem_rows().window(snapshot.tick.unwrap_or(0), u64::MAX);
            if !window.rows.is_empty() {
                match peers.iter_mut().find(|p| p.voter == own) {
                    Some(peer) => merge_rows(&mut peer.rows, window),
                    None => peers.push(BundlePeer {
                        voter: own,
                        rows: window,
                    }),
                }
            }
        }
        let verdict = host.desync_reports().1.cloned();
        let checkpoint = verdict.as_ref().map_or(host.tick(), |r| r.checkpoint);
        let mut bundle = Self {
            format_version: DESYNC_BUNDLE_FORMAT_VERSION,
            game: BundleGame::of::<G>(),
            writer: BundleWriter::Host,
            checkpoint,
            snapshot,
            frames,
            peers,
            verdict,
        };
        bundle.validate()?;
        Ok(bundle)
    }

    /// A diverged seat's bundle, written when it is told to take another state
    /// (`SeatEvent::Resync`) and before it does: `snapshot` (its state at the last agreed
    /// checkpoint, the `from` of the `SeatEvent::SelfCheck` before it), the frames after it and
    /// its own rows. The replay compares those rows with a fresh run of the same inputs.
    pub fn from_seat<G: NetGame>(
        seat: &LockstepSeat<G>,
        snapshot: BundleSnapshot,
    ) -> Result<Self, BundleError> {
        let first = snapshot.first_tick();
        let frames = frames_of(seat.frames_since(first))?;
        let rows = seat.subsystem_rows().window(snapshot.tick.unwrap_or(0), u64::MAX);
        let checkpoint = frames.last().map_or(first, |f| f.tick);
        let mut bundle = Self {
            format_version: DESYNC_BUNDLE_FORMAT_VERSION,
            game: BundleGame::of::<G>(),
            writer: BundleWriter::Seat(seat.seat().0),
            checkpoint,
            snapshot,
            frames,
            peers: vec![BundlePeer {
                voter: Voter::Seat(seat.seat()),
                rows,
            }],
            verdict: None,
        };
        bundle.validate()?;
        Ok(bundle)
    }

    /// Sorts the lists a reader walks (seats in each frame, peers) and checks every count, length
    /// and order against the `MAX_*` limits. [`Self::from_ron`] calls it.
    pub fn validate(&mut self) -> Result<(), BundleError> {
        let bad = |what: String| Err(BundleError::Invalid(what));
        if self.format_version != DESYNC_BUNDLE_FORMAT_VERSION {
            return Err(BundleError::Version {
                found: self.format_version,
            });
        }
        if self.game.game_id.is_empty() || self.game.game_id.len() > MAX_GAME_ID_BYTES {
            return bad(format!(
                "game_id is {} bytes (1..={MAX_GAME_ID_BYTES})",
                self.game.game_id.len()
            ));
        }
        if self.game.engine_version.len() > MAX_TEXT_BYTES {
            return bad(format!("engine_version is longer than {MAX_TEXT_BYTES} bytes"));
        }
        if self.snapshot.data.0.len() > MAX_SNAPSHOT_BYTES {
            return bad(format!("the snapshot is longer than {MAX_SNAPSHOT_BYTES} bytes"));
        }
        if self.frames.len() > MAX_BUNDLE_FRAMES {
            return bad(format!(
                "{} frames (at most {MAX_BUNDLE_FRAMES})",
                self.frames.len()
            ));
        }
        let mut expect = self.snapshot.first_tick();
        for frame in &mut self.frames {
            if frame.tick != expect {
                return bad(format!(
                    "frame tick {} where tick {expect} was due (frames follow the snapshot without a gap)",
                    frame.tick
                ));
            }
            expect = expect.saturating_add(1);
            if frame.inputs.len() > MAX_BUNDLE_SEATS || frame.membership.len() > MAX_BUNDLE_SEATS {
                return bad(format!("tick {}: more than {MAX_BUNDLE_SEATS} seats", frame.tick));
            }
            // Seat order is the step order on every path (DO27's offline replay walked a map).
            frame.inputs.sort_by_key(|i| i.seat);
            if frame.inputs.windows(2).any(|w| w[0].seat == w[1].seat) {
                return bad(format!("tick {}: a seat is listed twice", frame.tick));
            }
            if let Some(i) = frame.inputs.iter().find(|i| {
                i.input
                    .as_ref()
                    .is_some_and(|b| b.0.len() > MAX_BUNDLE_INPUT_BYTES)
            }) {
                return bad(format!(
                    "tick {} seat {}: an input longer than {MAX_BUNDLE_INPUT_BYTES} bytes",
                    frame.tick, i.seat
                ));
            }
            frame.membership.sort_by_key(|m| (m.seat, m.joins));
        }
        if self.peers.len() > MAX_BUNDLE_PEERS {
            return bad(format!(
                "{} peers with rows (at most {MAX_BUNDLE_PEERS})",
                self.peers.len()
            ));
        }
        self.peers.sort_by_key(|p| p.voter);
        if self.peers.windows(2).any(|w| w[0].voter == w[1].voter) {
            return bad("a peer is listed twice".to_owned());
        }
        for peer in &self.peers {
            check_rows(peer)?;
        }
        if let Some(report) = &self.verdict {
            let long = report.subsystems.len() > MAX_SUBSYSTEMS
                || report
                    .subsystems
                    .iter()
                    .chain(&report.subsystem)
                    .any(|s| s.len() > MAX_SUBSYSTEM_NAME_BYTES);
            if long {
                return bad("the verdict names too many or too long subsystems".to_owned());
            }
        }
        Ok(())
    }

    /// Parses and validates bundle text (untrusted input: never panics, see the module docs).
    pub fn from_ron(text: &str) -> Result<Self, BundleError> {
        let _span = tracing::debug_span!("regia_net::desync_bundle_decode", bytes = text.len()).entered();
        if text.len() > MAX_BUNDLE_BYTES {
            return Err(BundleError::TooLarge(text.len()));
        }
        let head: Head = ron::from_str(text).map_err(|e| BundleError::Parse(e.to_string()))?;
        if head.format_version != DESYNC_BUNDLE_FORMAT_VERSION {
            return Err(BundleError::Version {
                found: head.format_version,
            });
        }
        let mut bundle: Self = ron::from_str(text).map_err(|e| BundleError::Parse(e.to_string()))?;
        bundle.validate()?;
        Ok(bundle)
    }

    /// The bundle as RON text.
    pub fn to_ron(&self) -> Result<String, BundleError> {
        let pretty = regia_core::ron_pretty().depth_limit(5).struct_names(true);
        ron::ser::to_string_pretty(self, pretty).map_err(|e| BundleError::Parse(e.to_string()))
    }

    /// The file name: `<game>-<writer>-<checkpoint>.desync.ron`, with every character outside
    /// `[A-Za-z0-9._-]` of the game id written as `_`.
    pub fn file_name(&self) -> String {
        let game: String = self
            .game
            .game_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let writer = match self.writer {
            BundleWriter::Host => "host".to_owned(),
            BundleWriter::Seat(seat) => format!("seat{seat}"),
        };
        format!("{game}-{writer}-{}{DESYNC_BUNDLE_SUFFIX}", self.checkpoint)
    }

    /// Writes the bundle into `dir` (created if needed) as [`Self::file_name`], atomically
    /// (a temporary file renamed into place). Returns the path.
    pub fn write_to_dir(&self, dir: &Path) -> Result<PathBuf, BundleError> {
        let text = self.to_ron()?;
        let io = |what: &str, path: &Path, e: std::io::Error| {
            BundleError::Io(format!("{what} {}: {e}", path.display()))
        };
        std::fs::create_dir_all(dir).map_err(|e| io("creating", dir, e))?;
        let path = dir.join(self.file_name());
        let tmp = dir.join(format!(".{}.{}.tmp", self.file_name(), std::process::id()));
        std::fs::write(&tmp, text).map_err(|e| io("writing", &tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            io("renaming into", &path, e)
        })?;
        Ok(path)
    }

    /// Reads and validates a bundle file (at most [`MAX_BUNDLE_BYTES`]).
    pub fn read(path: &Path) -> Result<Self, BundleError> {
        let io = |e: std::io::Error| BundleError::Io(format!("reading {}: {e}", path.display()));
        let len = std::fs::metadata(path).map_err(io)?.len();
        if len > MAX_BUNDLE_BYTES as u64 {
            return Err(BundleError::TooLarge(usize::try_from(len).unwrap_or(usize::MAX)));
        }
        let text = std::fs::read_to_string(path).map_err(io)?;
        Self::from_ron(&text)
    }
}

fn check_rows(peer: &BundlePeer) -> Result<(), BundleError> {
    let bad = |what: String| Err(BundleError::Invalid(format!("{}: {what}", peer.voter)));
    let rows = &peer.rows;
    if rows.names.len() > MAX_SUBSYSTEMS {
        return bad(format!(
            "{} subsystem names (at most {MAX_SUBSYSTEMS})",
            rows.names.len()
        ));
    }
    if rows.names.iter().any(|n| n.len() > MAX_SUBSYSTEM_NAME_BYTES) {
        return bad(format!(
            "a subsystem name longer than {MAX_SUBSYSTEM_NAME_BYTES} bytes"
        ));
    }
    if rows.rows.len() > MAX_BUNDLE_ROWS {
        return bad(format!("{} rows (at most {MAX_BUNDLE_ROWS})", rows.rows.len()));
    }
    if rows
        .rows
        .iter()
        .any(|r: &SubsystemRow| r.hashes.len() != rows.names.len())
    {
        return bad("a row whose hash count differs from the names".to_owned());
    }
    if rows.rows.windows(2).any(|w| w[0].tick >= w[1].tick) {
        return bad("rows not strictly ascending by tick".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DesyncBundle {
        DesyncBundle {
            format_version: DESYNC_BUNDLE_FORMAT_VERSION,
            game: BundleGame {
                game_id: "test.bundle".into(),
                protocol_version: 3,
                protocol_id: 0x1234,
                engine_version: "0.0.0".into(),
            },
            writer: BundleWriter::Host,
            checkpoint: 30,
            snapshot: BundleSnapshot::at(9, vec![1, 2, 0xff]),
            frames: (10..12)
                .map(|tick| BundleFrame {
                    tick,
                    inputs: vec![
                        BundleInput { seat: 1, input: None },
                        BundleInput {
                            seat: 0,
                            input: Some(HexBytes(vec![tick as u8])),
                        },
                    ],
                    membership: Vec::new(),
                })
                .collect(),
            peers: vec![BundlePeer {
                voter: Voter::Seat(crate::lockstep::SeatId(1)),
                rows: ProbeRows {
                    names: vec!["a".into()],
                    rows: vec![SubsystemRow {
                        tick: 10,
                        world: 5,
                        hashes: vec![6],
                    }],
                },
            }],
            verdict: None,
        }
    }

    /// The reader round-trips what the writer wrote and sorts each frame's seats.
    #[test]
    fn a_bundle_round_trips_and_its_seats_come_back_sorted() {
        let bundle = sample();
        let text = bundle.to_ron().expect("ron");
        assert!(text.contains("\"0a\""), "{text}");
        let back = DesyncBundle::from_ron(&text).expect("decode");
        assert_eq!(back.frames[0].inputs[0].seat, 0, "sorted by seat");
        assert_eq!(back.frames[0].input(0), Some(&[10u8][..]));
        assert_eq!(back.frames[0].input(1), None);
        assert_eq!(back.snapshot.data.0, [1, 2, 0xff]);
        let mut sorted = bundle;
        sorted.validate().expect("valid");
        assert_eq!(back, sorted);
    }

    /// Every refusal names what was wrong, with its code.
    #[test]
    fn refusals_carry_codes() {
        let mut gap = sample();
        gap.frames[1].tick = 12;
        assert_eq!(gap.validate().map_err(|e| e.code()), Err("desync_bundle_invalid"));
        let text = sample()
            .to_ron()
            .expect("ron")
            .replace("format_version: 1", "format_version: 2");
        let err = DesyncBundle::from_ron(&text).expect_err("future version");
        assert_eq!(err.code(), "desync_bundle_version");
        assert!(err.to_string().contains("format_version 2"), "{err}");
        assert_eq!(
            DesyncBundle::from_ron("(").map_err(|e| e.code()),
            Err("desync_bundle_parse")
        );
        let odd = sample().to_ron().expect("ron").replace("\"0a\"", "\"0a0\"");
        assert_eq!(
            DesyncBundle::from_ron(&odd).map_err(|e| e.code()),
            Err("desync_bundle_parse")
        );
        let huge = " ".repeat(MAX_BUNDLE_BYTES + 1);
        assert_eq!(
            DesyncBundle::from_ron(&huge).map_err(|e| e.code()),
            Err("desync_bundle_too_large")
        );
        let mut twice = sample();
        twice.frames[0].inputs[0].seat = 0;
        assert_eq!(
            twice.validate().map_err(|e| e.code()),
            Err("desync_bundle_invalid")
        );
    }

    /// `write_to_dir` and `read` round-trip through a file named after the game, writer and
    /// checkpoint.
    #[test]
    fn a_bundle_file_round_trips() {
        // pid + counter (regia_test_utils depends on this crate, so its helper is not used here).
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-desync-bundle-{}-{n}", std::process::id()));
        let path = sample().write_to_dir(&dir).expect("write");
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("test.bundle-host-30.desync.ron")
        );
        let back = DesyncBundle::read(&path).expect("read");
        assert_eq!(back.checkpoint, 30);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
