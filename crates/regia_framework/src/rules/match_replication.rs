// SPDX-License-Identifier: MPL-2.0
//! Replicated match state for client/server games (feature `net`; `book/agents/match-rules.md`,
//! "Replicated match state").
//!
//! The server keeps one [`MatchStateEntity`] (marked `NetPersistAcrossTravel`, spec 77 § 6.2)
//! that carries three replicated views of its [`MatchRules`] session:
//!
//! - [`MatchPhaseView`]: phase, round, match tick, time left, pause and pauser, the scheduled
//!   start, match over and the winner;
//! - [`TeamScoresView`]: the board's team scores, round wins and team labels, at most
//!   `max_teams + 1` entries;
//! - [`ObjectiveViews`]: each flag and hill (id, kind, team, state, position, capture progress),
//!   at most `max_objectives` entries.
//!
//! Each player entity with [`TeamId`] and [`LedgerSlot`] gets a [`PlayerScoreView`] from its
//! seat's ledger row. Each [`RulesEvent`] the server's adapter sends as a [`MatchEvent`] also
//! travels to the clients as a reliable ordered [`MatchEventMsg`] (at most
//! [`MatchStateSettings::max_event_bytes`]); a client writes it as the same [`MatchEvent`]
//! message, so a HUD feed or an announcer reads one type on a dedicated server, a listen server
//! and a client. A client never steps rules: [`ReplicatedMatchRules`] answers
//! [`GameRules::scoreboard`] and [`GameRules::is_match_over`] from the views.
//!
//! Late join: the views arrive with the joiner's first snapshot. Events sent before the join are
//! not replayed.
//!
//! Lockstep games do not use this path: every peer steps its own session.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_net::bits::{BitReader, BitWriter, DecodeError};
use regia_net::codec::{CodecContext, LayoutHasher, ReplicateCodec};
use regia_net::replication::{ReplicationRegistry, register_replicated};
use regia_net::runtime::NetNode;
use regia_net::{
    BroadcastMessage, Direction, NetMessage, NetMessageAppExt, NetMessages, NetMode, NetOwnedLocally,
    NetPersistAcrossTravel, NetSender, NetSet, Replicate, ReplicateOptions, Replicated, TransformReplication,
};
use regia_rules::elimination::OutCause;
use regia_rules::match_flow::{SeatId, seat_label};
use regia_rules::objective::{AutoReturnCause, EndReason, ObjectiveKind, RulesEvent, ScoreReason};
use regia_rules::phases::MatchPhase;
use regia_rules::playlist::SkipReason;
use regia_rules::session::MatchSession;
use regia_rules::waves::CoopCause;
use regia_rules::zone::ZoneStage;

use super::{
    GameRules, LedgerSlot, MatchEvent, MatchRules, MatchRulesSet, RulesCtx, ScoreCell, ScoreSheet,
    ScoreSheetColumn, ScoreSheetRow, ScoreSheetTeam, SpawnRequest, TeamId,
};

/// Most team entries a [`TeamScoresView`] decodes (team `0` included). A server with more teams
/// sends the first ones.
pub const MATCH_VIEW_MAX_TEAMS: usize = 64;

/// Most objectives an [`ObjectiveViews`] decodes. A server with more sends the first ones.
pub const MATCH_VIEW_MAX_OBJECTIVES: usize = 256;

/// Longest team label a [`TeamScoresView`] carries, in bytes (longer labels are cut on send).
pub const MATCH_VIEW_MAX_LABEL: usize = 64;

/// Longest pause banner name a [`MatchPhaseView`] carries, in bytes.
pub const MATCH_VIEW_MAX_PAUSE_NAME: usize = 64;

/// Hard wire bound of one [`MatchEventMsg`] payload: a longer length prefix is a decode error
/// the net layer drops. [`MatchStateSettings::max_event_bytes`] is the per-game cap below it.
pub const MATCH_EVENT_WIRE_LIMIT: usize = 4096;

/// Default of [`MatchStateSettings::clock_resync_ticks`]: half a second at 60 Hz.
pub const DEFAULT_CLOCK_RESYNC_TICKS: u32 = 30;

/// Default of [`MatchStateSettings::max_event_bytes`].
pub const DEFAULT_MAX_EVENT_BYTES: usize = 256;

/// The payload format of a [`MatchEventMsg`]. A payload whose first byte differs is dropped and
/// counted as malformed.
pub const MATCH_EVENT_FORMAT: u8 = 1;

/// Longest string field (pause name, map id, mode) an encoded event carries, in bytes.
const EVENT_MAX_STR: usize = 128;
/// Most winning teams a `victory` event carries on the wire (a team id is one byte).
const EVENT_MAX_TEAMS: usize = 256;

/// Capture progress of a full bar in [`ObjectiveView::capture_progress`].
pub const CAPTURE_PROGRESS_FULL: u16 = 1000;

// ---- views --------------------------------------------------------------------------------------

/// Marks the server's single match-state entity (and its client copy). It carries
/// [`MatchPhaseView`], [`TeamScoresView`] and [`ObjectiveViews`].
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MatchStateEntity;

impl ReplicateCodec for MatchStateEntity {
    fn encode(&self, _w: &mut BitWriter, _cx: &CodecContext) {}

    fn decode(_r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        Ok(Self)
    }

    fn wire_eq(&self, _other: &Self, _cx: &CodecContext) -> bool {
        true
    }

    fn layout(h: &mut LayoutHasher) {
        h.write_str("regia_framework::rules::MatchStateEntity");
    }
}

/// A seat on the wire: team and slot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Replicate)]
pub struct SeatView {
    /// The team.
    pub team: u8,
    /// The slot on the team.
    pub slot: u8,
}

impl From<SeatId> for SeatView {
    fn from(seat: SeatId) -> Self {
        Self {
            team: seat.team,
            slot: seat.slot,
        }
    }
}

/// Phase, clock and pause of the server's match.
///
/// The clock fields (`match_tick`, `time_left_ticks`, `flow_step`, `net_tick`) are rewritten
/// every [`MatchStateSettings::clock_resync_ticks`] fixed ticks, and whenever another field
/// changes. Between two writes a client reads [`MatchPhaseView::time_left_at`] and
/// [`MatchPhaseView::match_tick_at`] with its newest applied server tick.
#[derive(Component, Debug, Clone, Default, PartialEq, Eq, Replicate)]
pub struct MatchPhaseView {
    /// The phase: the index in [`MatchPhase::ALL`] (read it with [`MatchPhaseView::phase`]).
    pub phase: u8,
    /// The round, from 1, with rounds on; `0` with rounds off.
    pub round: u32,
    /// The match tick when the clock fields were written.
    pub match_tick: u64,
    /// Ticks left on the time limit (or a capped overtime) when the clock fields were written.
    /// `None` when no clock runs down.
    pub time_left_ticks: Option<u64>,
    /// The match is paused (the match tick is frozen).
    pub paused: bool,
    /// The seat that paused a friendly match; `None` while running or under a host pause.
    pub pauser: Option<SeatView>,
    /// The name on the pause banner (empty while running or under a host pause).
    #[replicate(max_len = 64)]
    pub paused_by: String,
    /// Between matches: the ready gate is open.
    pub between: bool,
    /// The flow step the next match starts on, once every seat that must ready did.
    pub starts_at: Option<u64>,
    /// The flow step when the clock fields were written (with `starts_at`, the launch countdown).
    pub flow_step: u64,
    /// The board says the match is over.
    pub match_over: bool,
    /// The winning team once over; `0` for none or a draw.
    pub winner: u8,
    /// Why it ended: [`end_reason_code`] (read it with [`MatchPhaseView::end_reason`]); `0` while
    /// running.
    pub end_reason: u8,
    /// The server's net tick when the clock fields were written.
    pub net_tick: u32,
}

impl MatchPhaseView {
    /// The phase (`Live` for an index this build does not know).
    pub fn phase(&self) -> MatchPhase {
        MatchPhase::ALL
            .get(usize::from(self.phase))
            .copied()
            .unwrap_or(MatchPhase::Live)
    }

    /// The end reason as its lowercase string (`""` while running or for a code this build does
    /// not know).
    pub fn end_reason(&self) -> &'static str {
        end_reason_from_code(self.end_reason).map_or("", EndReason::as_str)
    }

    /// Whether the time limit counts down on each tick right now (live or overtime, running, not
    /// over).
    pub fn clock_runs(&self) -> bool {
        self.phase().is_play() && !self.paused && !self.match_over && !self.between
    }

    /// Ticks the server has run since the clock fields were written, seen from `net_tick` (a
    /// client's newest applied server tick).
    fn elapsed(&self, net_tick: u32) -> u64 {
        u64::from(net_tick.saturating_sub(self.net_tick))
    }

    /// Time left at server tick `net_tick`: the written value, minus the ticks since it was
    /// written while the clock runs.
    pub fn time_left_at(&self, net_tick: u32) -> Option<u64> {
        let left = self.time_left_ticks?;
        if self.clock_runs() {
            Some(left.saturating_sub(self.elapsed(net_tick)))
        } else {
            Some(left)
        }
    }

    /// The match tick at server tick `net_tick` (frozen while paused or between matches).
    pub fn match_tick_at(&self, net_tick: u32) -> u64 {
        if self.paused || self.between {
            self.match_tick
        } else {
            self.match_tick.saturating_add(self.elapsed(net_tick))
        }
    }

    fn same_except_clock(&self, other: &Self) -> bool {
        let strip = |v: &Self| Self {
            match_tick: 0,
            time_left_ticks: None,
            flow_step: 0,
            net_tick: 0,
            ..v.clone()
        };
        strip(self) == strip(other)
    }
}

/// One team's label on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq, Replicate)]
pub struct TeamLabelView {
    /// The team id.
    pub id: u8,
    /// The team's name key (empty for an unnamed team).
    #[replicate(max_len = 64)]
    pub name: String,
    /// Color as RGBA bytes packed `r << 24 | g << 16 | b << 8 | a`.
    pub color: u32,
}

/// The board's team scores and round wins, indexed by team id (`0` is neutral), plus the team
/// labels. At most `max_teams + 1` entries (and [`MATCH_VIEW_MAX_TEAMS`]).
#[derive(Component, Debug, Clone, Default, PartialEq, Eq, Replicate)]
pub struct TeamScoresView {
    /// Score per team id.
    #[replicate(max_len = 64)]
    pub scores: Vec<u32>,
    /// Round wins per team id (empty with rounds off).
    #[replicate(max_len = 64)]
    pub round_wins: Vec<u32>,
    /// The session's team definitions, sorted by id.
    #[replicate(max_len = 64)]
    pub labels: Vec<TeamLabelView>,
}

impl TeamScoresView {
    /// The score of `team` (`0` for a team the view does not hold).
    pub fn score(&self, team: u8) -> u32 {
        self.scores.get(usize::from(team)).copied().unwrap_or(0)
    }
}

/// Where an objective is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Replicate)]
pub enum ObjectiveViewState {
    /// A flag at its base.
    #[default]
    AtBase,
    /// A flag carried by unit `carrier` (an entity index on the server).
    Carried {
        /// The carrier's unit id.
        carrier: u32,
        /// The carrier's seat when its ledger row is bound to the unit: a client follows the entity
        /// whose [`PlayerScoreView`] names it ([`objective_position`]).
        seat: Option<SeatView>,
    },
    /// A flag lying where it dropped.
    Dropped,
    /// The hill (a control volume; it never moves).
    Hill,
    /// A capture zone (ADR-8649; carried by the v0.7 roll-up, ADR-8731): its owner (`0` while
    /// neutral), the team whose bar it holds (`0` for none) and whether it is locked now.
    /// [`ObjectiveView::capture_progress`] is that bar.
    Zone {
        /// The owning team; `0` while neutral.
        owner: u8,
        /// The team whose partial progress the zone holds; `0` for none.
        capturing: u8,
        /// Locked now: it takes no progress and scores nothing.
        locked: bool,
    },
}

/// One objective on the wire.
#[derive(Debug, Clone, Default, PartialEq, Replicate)]
pub struct ObjectiveView {
    /// The objective's entity id on the board (`Objective::entity_id`).
    pub id: u32,
    /// `0` flag, `1` hill, `2` capture zone (read it with [`ObjectiveView::kind`]).
    pub kind: u8,
    /// The flag's team; `0` for the hill; a zone's owner (`0` while neutral).
    pub team: u8,
    /// Where it is.
    pub state: ObjectiveViewState,
    /// Its position. A carried flag shows its base here, so the list does not change while the
    /// carrier runs: read [`objective_position`] for where it is.
    pub position: Vec3,
    /// Progress in thousandths ([`CAPTURE_PROGRESS_FULL`] is a full bar), in steps of 100: how far
    /// a dropped flag's return timer has run (so the list changes at most ten times per return), or
    /// how full a capture zone's bar is. `0` for a flag at base or carried, and for the hill (the
    /// core has no capture bar for it).
    pub capture_progress: u16,
}

impl ObjectiveView {
    /// The kind.
    pub fn kind(&self) -> ObjectiveKind {
        match self.kind {
            1 => ObjectiveKind::Hill,
            2 => ObjectiveKind::Zone,
            _ => ObjectiveKind::Flag,
        }
    }
}

/// The board's objectives in placement order, at most `max_objectives` (and
/// [`MATCH_VIEW_MAX_OBJECTIVES`]).
#[derive(Component, Debug, Clone, Default, PartialEq, Replicate)]
pub struct ObjectiveViews {
    /// The objectives.
    #[replicate(max_len = 256)]
    pub items: Vec<ObjectiveView>,
}

/// A player entity's ledger row on the wire (server: written from the seat its [`TeamId`] and
/// [`LedgerSlot`] name).
#[derive(Component, Debug, Clone, Default, PartialEq, Eq, Replicate)]
pub struct PlayerScoreView {
    /// The seat's team.
    pub team: u8,
    /// The seat's slot.
    pub slot: u8,
    /// The ledger row's id (stable for the seat's stay; the score-sheet row id).
    pub row_id: u64,
    /// Kills.
    pub kills: u32,
    /// Deaths.
    pub deaths: u32,
    /// Assists.
    pub assists: u32,
    /// Objective points.
    pub objective_points: u32,
    /// Score.
    pub score: i64,
}

// ---- codec of TeamId ----------------------------------------------------------------------------

impl ReplicateCodec for TeamId {
    fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
        w.write_bits(u64::from(self.0), 8);
    }

    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        Ok(Self(r.read_bits(8)? as u8))
    }

    fn wire_eq(&self, other: &Self, _cx: &CodecContext) -> bool {
        self == other
    }

    fn layout(h: &mut LayoutHasher) {
        h.write_str("regia_framework::rules::TeamId");
        h.write_u64(8);
    }
}

// ---- events -------------------------------------------------------------------------------------

/// Server to every client: one encoded [`RulesEvent`] (reliable, ordered). The payload is
/// [`encode_match_event`]'s bytes; a client decodes it with [`decode_match_event`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MatchEventMsg {
    /// The encoded event.
    pub payload: Vec<u8>,
}

impl ReplicateCodec for MatchEventMsg {
    fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
        let n = self.payload.len().min(MATCH_EVENT_WIRE_LIMIT);
        w.write_varint(n as u64);
        w.write_bytes(&self.payload[..n]);
    }

    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        let n = r.read_len(MATCH_EVENT_WIRE_LIMIT)?;
        Ok(Self {
            payload: r.read_bytes(n)?,
        })
    }

    fn layout(h: &mut LayoutHasher) {
        h.write_str("regia_framework::rules::MatchEventMsg");
        h.write_u64(MATCH_EVENT_WIRE_LIMIT as u64);
        h.write_u64(u64::from(MATCH_EVENT_FORMAT));
    }
}

impl NetMessage for MatchEventMsg {
    const DIRECTION: Direction = Direction::Broadcast;

    fn message_id() -> &'static str {
        "regia_framework::rules::MatchEventMsg"
    }
}

impl BroadcastMessage for MatchEventMsg {}

/// Why an encoded event could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchEventDecodeError {
    /// The payload is longer than the cap.
    Oversized,
    /// The format byte, the kind or a field is not one this build reads, or bytes are left over.
    Malformed,
}

fn phase_code(phase: MatchPhase) -> u8 {
    MatchPhase::ALL
        .iter()
        .position(|p| *p == phase)
        .map_or(u8::MAX, |i| i as u8)
}

/// The wire code of an end reason (`1..=7`; `0` is "none"; `255` a reason this build cannot name).
pub fn end_reason_code(reason: EndReason) -> u8 {
    match reason {
        EndReason::CaptureLimitReached => 1,
        EndReason::HillScoreLimitReached => 2,
        EndReason::Ended => 3,
        EndReason::TimeLimit => 4,
        EndReason::Draw => 5,
        EndReason::Forfeit => 6,
        EndReason::RoundsWon => 7,
        // Capture zones (ADR-8649) and the genre reasons (GR-11.3, GR-11.4), numbered at the v0.7
        // roll-up (ADR-8731) as `regia_rules::proof` tags them.
        EndReason::ZoneScoreLimitReached => 8,
        EndReason::ZonesCaptured => 9,
        EndReason::LastStanding => 10,
        EndReason::Destroyed => 11,
        EndReason::Held => 12,
        EndReason::ScoreReached => 13,
        EndReason::Surrendered => 14,
        _ => u8::MAX,
    }
}

fn end_reason_from_code(code: u8) -> Option<EndReason> {
    Some(match code {
        1 => EndReason::CaptureLimitReached,
        2 => EndReason::HillScoreLimitReached,
        3 => EndReason::Ended,
        4 => EndReason::TimeLimit,
        5 => EndReason::Draw,
        6 => EndReason::Forfeit,
        7 => EndReason::RoundsWon,
        8 => EndReason::ZoneScoreLimitReached,
        9 => EndReason::ZonesCaptured,
        10 => EndReason::LastStanding,
        11 => EndReason::Destroyed,
        12 => EndReason::Held,
        13 => EndReason::ScoreReached,
        14 => EndReason::Surrendered,
        _ => return None,
    })
}

fn write_str(w: &mut BitWriter, s: &str) {
    let mut end = s.len().min(EVENT_MAX_STR);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    w.write_varint(end as u64);
    w.write_bytes(&s.as_bytes()[..end]);
}

fn read_str(r: &mut BitReader) -> Result<String, DecodeError> {
    let n = r.read_len(EVENT_MAX_STR)?;
    String::from_utf8(r.read_bytes(n)?).map_err(|_| DecodeError::Invalid("utf-8"))
}

fn read_u8(r: &mut BitReader) -> Result<u8, DecodeError> {
    Ok(r.read_bits(8)? as u8)
}

fn read_u32(r: &mut BitReader) -> Result<u32, DecodeError> {
    u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("u32"))
}

fn write_pos(w: &mut BitWriter, p: [f32; 3]) {
    for v in p {
        w.write_f32(v);
    }
}

fn read_pos(r: &mut BitReader) -> Result<[f32; 3], DecodeError> {
    Ok([r.read_f32()?, r.read_f32()?, r.read_f32()?])
}

/// Encodes one event: the format byte [`MATCH_EVENT_FORMAT`], a kind byte, then the fields
/// (varints, raw `f32`s, length-prefixed strings of at most 128 bytes). Every core event has a
/// kind byte (ADR-8731; `RowSkipped` is kind 38, ADR-8881) except `ZoneDamage`, the server's
/// per-unit damage instruction, which is `None` like a variant this build does not know (the core
/// may add variants).
pub fn encode_match_event(event: &RulesEvent) -> Option<Vec<u8>> {
    let mut w = BitWriter::with_capacity(32);
    w.write_bits(u64::from(MATCH_EVENT_FORMAT), 8);
    match event {
        RulesEvent::FlagTaken {
            tick,
            flag,
            unit,
            team,
            flag_team,
        } => {
            w.write_bits(1, 8);
            w.write_varint(*tick);
            w.write_varint(u64::from(*flag));
            w.write_varint(u64::from(*unit));
            w.write_bits(u64::from(*team), 8);
            w.write_bits(u64::from(*flag_team), 8);
        }
        RulesEvent::FlagDropped {
            tick,
            flag,
            unit,
            flag_team,
            position,
        } => {
            w.write_bits(2, 8);
            w.write_varint(*tick);
            w.write_varint(u64::from(*flag));
            w.write_varint(u64::from(*unit));
            w.write_bits(u64::from(*flag_team), 8);
            write_pos(&mut w, *position);
        }
        RulesEvent::FlagReturned {
            tick,
            flag,
            unit,
            flag_team,
        } => {
            w.write_bits(3, 8);
            w.write_varint(*tick);
            w.write_varint(u64::from(*flag));
            w.write_varint(u64::from(*unit));
            w.write_bits(u64::from(*flag_team), 8);
        }
        RulesEvent::FlagCaptured {
            tick,
            flag,
            unit,
            team,
            flag_team,
            score,
        } => {
            w.write_bits(4, 8);
            w.write_varint(*tick);
            w.write_varint(u64::from(*flag));
            w.write_varint(u64::from(*unit));
            w.write_bits(u64::from(*team), 8);
            w.write_bits(u64::from(*flag_team), 8);
            w.write_varint(u64::from(*score));
        }
        RulesEvent::HillPoint { tick, team, score } => {
            w.write_bits(5, 8);
            w.write_varint(*tick);
            w.write_bits(u64::from(*team), 8);
            w.write_varint(u64::from(*score));
        }
        RulesEvent::MatchOver { tick, team, reason } => {
            w.write_bits(6, 8);
            w.write_varint(*tick);
            w.write_bits(u64::from(*team), 8);
            w.write_bits(u64::from(end_reason_code(*reason)), 8);
        }
        RulesEvent::Paused { tick, by } => {
            w.write_bits(7, 8);
            w.write_varint(*tick);
            write_str(&mut w, by);
        }
        RulesEvent::Resumed { tick } => {
            w.write_bits(8, 8);
            w.write_varint(*tick);
        }
        RulesEvent::LaunchScheduled { at_step } => {
            w.write_bits(9, 8);
            w.write_varint(*at_step);
        }
        RulesEvent::LaunchCancelled { step } => {
            w.write_bits(10, 8);
            w.write_varint(*step);
        }
        RulesEvent::Launched {
            step,
            index,
            map_id,
            mode,
        } => {
            w.write_bits(11, 8);
            w.write_varint(*step);
            w.write_bool(index.is_some());
            if let Some(index) = index {
                w.write_varint(u64::from(*index));
            }
            write_str(&mut w, map_id);
            write_str(&mut w, mode);
        }
        RulesEvent::SeatLeft { step, team, slot } => {
            w.write_bits(12, 8);
            w.write_varint(*step);
            w.write_bits(u64::from(*team), 8);
            w.write_bits(u64::from(*slot), 8);
        }
        RulesEvent::PhaseChanged { step, phase, round } => {
            w.write_bits(13, 8);
            w.write_varint(*step);
            w.write_bits(u64::from(phase_code(*phase)), 8);
            w.write_varint(u64::from(*round));
        }
        RulesEvent::RoundOver {
            tick,
            round,
            team,
            reason,
        } => {
            w.write_bits(14, 8);
            w.write_varint(*tick);
            w.write_varint(u64::from(*round));
            w.write_bits(u64::from(*team), 8);
            w.write_bits(u64::from(end_reason_code(*reason)), 8);
        }
        RulesEvent::FlagAutoReturned {
            tick,
            flag,
            flag_team,
            cause,
        } => {
            w.write_bits(15, 8);
            w.write_varint(*tick);
            w.write_varint(u64::from(*flag));
            w.write_bits(u64::from(*flag_team), 8);
            let cause = match cause {
                AutoReturnCause::Timeout => 0,
                AutoReturnCause::DropCheck => 1,
            };
            w.write_bits(cause, 8);
        }
        RulesEvent::PlayerScored {
            tick,
            team,
            slot,
            reason,
            score,
        } => {
            w.write_bits(16, 8);
            w.write_varint(*tick);
            w.write_bits(u64::from(*team), 8);
            w.write_bits(u64::from(*slot), 8);
            let reason = match reason {
                ScoreReason::Kill => 0,
                ScoreReason::Death => 1,
                ScoreReason::Assist => 2,
                ScoreReason::Objective => 3,
                ScoreReason::Scripted => 4,
            };
            w.write_bits(reason, 8);
            w.write_zigzag(*score);
        }
        RulesEvent::RespawnAt { tick, team, slot } => seat_event(&mut w, 17, *tick, *team, *slot),
        RulesEvent::Eliminated { tick, team, slot } => seat_event(&mut w, 18, *tick, *team, *slot),
        RulesEvent::ZoneCaptured {
            tick,
            zone,
            team,
            previous,
        } => {
            w.write_bits(19, 8);
            w.write_varint(*tick);
            w.write_varint(u64::from(*zone));
            w.write_bits(u64::from(*team), 8);
            w.write_bits(u64::from(*previous), 8);
        }
        RulesEvent::ZonePoint {
            tick,
            zone,
            team,
            score,
        } => {
            w.write_bits(20, 8);
            w.write_varint(*tick);
            w.write_varint(u64::from(*zone));
            w.write_bits(u64::from(*team), 8);
            w.write_varint(u64::from(*score));
        }
        RulesEvent::ZoneActivated { tick, zone } => {
            w.write_bits(21, 8);
            w.write_varint(*tick);
            w.write_varint(u64::from(*zone));
        }
        RulesEvent::WaveStarted { tick, wave } => {
            w.write_bits(22, 8);
            w.write_varint(*tick);
            w.write_varint(u64::from(*wave));
        }
        RulesEvent::WaveSpawn {
            tick,
            wave,
            group,
            archetype,
            count,
            spawn_tag,
        } => {
            w.write_bits(23, 8);
            w.write_varint(*tick);
            w.write_varint(u64::from(*wave));
            w.write_varint(u64::from(*group));
            write_str(&mut w, archetype);
            w.write_varint(u64::from(*count));
            write_str(&mut w, spawn_tag);
        }
        RulesEvent::WaveCleared { tick, wave } => {
            w.write_bits(24, 8);
            w.write_varint(*tick);
            w.write_varint(u64::from(*wave));
        }
        RulesEvent::CoopEnded { tick, won, cause } => {
            w.write_bits(25, 8);
            w.write_varint(*tick);
            w.write_bool(*won);
            let cause = match cause {
                CoopCause::AllWavesCleared => 0,
                CoopCause::ObjectiveReached => 1,
                CoopCause::TeamWipe => 2,
                CoopCause::ObjectiveDestroyed => 3,
            };
            w.write_bits(cause, 8);
        }
        RulesEvent::RacerCheckpoint {
            tick,
            team,
            slot,
            checkpoint,
            lap,
        } => {
            seat_event(&mut w, 26, *tick, *team, *slot);
            w.write_varint(u64::from(*checkpoint));
            w.write_varint(u64::from(*lap));
        }
        RulesEvent::RacerLap {
            tick,
            team,
            slot,
            lap,
            lap_ticks,
            best,
        } => {
            seat_event(&mut w, 27, *tick, *team, *slot);
            w.write_varint(u64::from(*lap));
            w.write_varint(*lap_ticks);
            w.write_bool(*best);
        }
        RulesEvent::RacerMissedCheckpoint {
            tick,
            team,
            slot,
            checkpoint,
            expected,
        } => {
            seat_event(&mut w, 28, *tick, *team, *slot);
            w.write_varint(u64::from(*checkpoint));
            w.write_varint(u64::from(*expected));
        }
        RulesEvent::RacerWrongWay { tick, team, slot } => seat_event(&mut w, 29, *tick, *team, *slot),
        RulesEvent::RacerFinished {
            tick,
            team,
            slot,
            place,
        } => {
            seat_event(&mut w, 30, *tick, *team, *slot);
            w.write_varint(u64::from(*place));
        }
        RulesEvent::RacerDnf { tick, team, slot } => seat_event(&mut w, 31, *tick, *team, *slot),
        RulesEvent::ZonePhase {
            tick,
            phase,
            stage,
            center,
            radius,
            target_center,
            target_radius,
        } => {
            w.write_bits(32, 8);
            w.write_varint(*tick);
            w.write_varint(u64::from(*phase));
            let stage = match stage {
                ZoneStage::Idle => 0,
                ZoneStage::Waiting => 1,
                ZoneStage::Shrinking => 2,
                ZoneStage::Closed => 3,
            };
            w.write_bits(stage, 8);
            for v in [
                center[0],
                center[1],
                *radius,
                target_center[0],
                target_center[1],
                *target_radius,
            ] {
                w.write_f32(v);
            }
        }
        RulesEvent::TeamEliminated {
            tick,
            team,
            place,
            cause,
        } => {
            w.write_bits(34, 8);
            w.write_varint(*tick);
            w.write_bits(u64::from(*team), 8);
            w.write_bits(u64::from(*place), 8);
            let cause = match cause {
                OutCause::Eliminated => 0,
                OutCause::Surrendered => 1,
                OutCause::Destroyed => 2,
                OutCause::Host => 3,
            };
            w.write_bits(cause, 8);
        }
        RulesEvent::Victory { tick, teams, reason } => {
            w.write_bits(35, 8);
            w.write_varint(*tick);
            // A team id is a byte, so at most 256 distinct winners; more is not a real victory.
            let teams = &teams[..teams.len().min(EVENT_MAX_TEAMS)];
            w.write_varint(teams.len() as u64);
            for team in teams {
                w.write_bits(u64::from(*team), 8);
            }
            w.write_bits(u64::from(end_reason_code(*reason)), 8);
        }
        RulesEvent::SurrenderVote {
            tick,
            team,
            slot,
            yes,
        } => {
            seat_event(&mut w, 36, *tick, *team, *slot);
            w.write_bool(*yes);
        }
        RulesEvent::SurrenderFailed { tick, team, until } => {
            w.write_bits(37, 8);
            w.write_varint(*tick);
            w.write_bits(u64::from(*team), 8);
            w.write_varint(*until);
        }
        // R-04 (4), ADR-8881: a rotation skip, so a client's announcer can say why a map was
        // passed over.
        RulesEvent::RowSkipped {
            step,
            index,
            map_id,
            reason,
        } => {
            w.write_bits(38, 8);
            w.write_varint(*step);
            w.write_varint(u64::from(*index));
            write_str(&mut w, map_id);
            w.write_bits(u64::from(skip_reason_code(*reason)), 8);
        }
        // `ZoneDamage` (kind 33 is reserved for it) is a per-unit instruction for the server's
        // damage step; clients see its effect through replicated health, so it is never sent.
        _ => return None,
    }
    Some(w.finish())
}

/// The wire code of a rotation skip reason (kind 38).
fn skip_reason_code(reason: SkipReason) -> u8 {
    match reason {
        SkipReason::TooFewPlayers => 0,
        SkipReason::TooManyPlayers => 1,
        SkipReason::MissingContent => 2,
        SkipReason::Recent => 3,
    }
}

/// Kind byte, tick and seat of an event about one seat.
fn seat_event(w: &mut BitWriter, kind: u64, tick: u64, team: u8, slot: u8) {
    w.write_bits(kind, 8);
    w.write_varint(tick);
    w.write_bits(u64::from(team), 8);
    w.write_bits(u64::from(slot), 8);
}

fn decode_body(r: &mut BitReader) -> Result<RulesEvent, DecodeError> {
    if read_u8(r)? != MATCH_EVENT_FORMAT {
        return Err(DecodeError::Invalid("match event format"));
    }
    let reason = |code: u8| end_reason_from_code(code).ok_or(DecodeError::Invalid("end reason"));
    Ok(match read_u8(r)? {
        1 => RulesEvent::FlagTaken {
            tick: r.read_varint()?,
            flag: read_u32(r)?,
            unit: read_u32(r)?,
            team: read_u8(r)?,
            flag_team: read_u8(r)?,
        },
        2 => RulesEvent::FlagDropped {
            tick: r.read_varint()?,
            flag: read_u32(r)?,
            unit: read_u32(r)?,
            flag_team: read_u8(r)?,
            position: read_pos(r)?,
        },
        3 => RulesEvent::FlagReturned {
            tick: r.read_varint()?,
            flag: read_u32(r)?,
            unit: read_u32(r)?,
            flag_team: read_u8(r)?,
        },
        4 => RulesEvent::FlagCaptured {
            tick: r.read_varint()?,
            flag: read_u32(r)?,
            unit: read_u32(r)?,
            team: read_u8(r)?,
            flag_team: read_u8(r)?,
            score: read_u32(r)?,
        },
        5 => RulesEvent::HillPoint {
            tick: r.read_varint()?,
            team: read_u8(r)?,
            score: read_u32(r)?,
        },
        6 => RulesEvent::MatchOver {
            tick: r.read_varint()?,
            team: read_u8(r)?,
            reason: reason(read_u8(r)?)?,
        },
        7 => RulesEvent::Paused {
            tick: r.read_varint()?,
            by: read_str(r)?,
        },
        8 => RulesEvent::Resumed {
            tick: r.read_varint()?,
        },
        9 => RulesEvent::LaunchScheduled {
            at_step: r.read_varint()?,
        },
        10 => RulesEvent::LaunchCancelled {
            step: r.read_varint()?,
        },
        11 => RulesEvent::Launched {
            step: r.read_varint()?,
            index: if r.read_bool()? { Some(read_u32(r)?) } else { None },
            map_id: read_str(r)?,
            mode: read_str(r)?,
        },
        12 => RulesEvent::SeatLeft {
            step: r.read_varint()?,
            team: read_u8(r)?,
            slot: read_u8(r)?,
        },
        13 => RulesEvent::PhaseChanged {
            step: r.read_varint()?,
            phase: *MatchPhase::ALL
                .get(usize::from(read_u8(r)?))
                .ok_or(DecodeError::Invalid("phase"))?,
            round: read_u32(r)?,
        },
        14 => RulesEvent::RoundOver {
            tick: r.read_varint()?,
            round: read_u32(r)?,
            team: read_u8(r)?,
            reason: reason(read_u8(r)?)?,
        },
        15 => RulesEvent::FlagAutoReturned {
            tick: r.read_varint()?,
            flag: read_u32(r)?,
            flag_team: read_u8(r)?,
            cause: match read_u8(r)? {
                0 => AutoReturnCause::Timeout,
                1 => AutoReturnCause::DropCheck,
                _ => return Err(DecodeError::Invalid("auto-return cause")),
            },
        },
        16 => RulesEvent::PlayerScored {
            tick: r.read_varint()?,
            team: read_u8(r)?,
            slot: read_u8(r)?,
            reason: match read_u8(r)? {
                0 => ScoreReason::Kill,
                1 => ScoreReason::Death,
                2 => ScoreReason::Assist,
                3 => ScoreReason::Objective,
                4 => ScoreReason::Scripted,
                _ => return Err(DecodeError::Invalid("score reason")),
            },
            score: r.read_zigzag()?,
        },
        17 => RulesEvent::RespawnAt {
            tick: r.read_varint()?,
            team: read_u8(r)?,
            slot: read_u8(r)?,
        },
        18 => RulesEvent::Eliminated {
            tick: r.read_varint()?,
            team: read_u8(r)?,
            slot: read_u8(r)?,
        },
        19 => RulesEvent::ZoneCaptured {
            tick: r.read_varint()?,
            zone: read_u32(r)?,
            team: read_u8(r)?,
            previous: read_u8(r)?,
        },
        20 => RulesEvent::ZonePoint {
            tick: r.read_varint()?,
            zone: read_u32(r)?,
            team: read_u8(r)?,
            score: read_u32(r)?,
        },
        21 => RulesEvent::ZoneActivated {
            tick: r.read_varint()?,
            zone: read_u32(r)?,
        },
        22 => RulesEvent::WaveStarted {
            tick: r.read_varint()?,
            wave: read_u32(r)?,
        },
        23 => RulesEvent::WaveSpawn {
            tick: r.read_varint()?,
            wave: read_u32(r)?,
            group: read_u32(r)?,
            archetype: read_str(r)?,
            count: read_u32(r)?,
            spawn_tag: read_str(r)?,
        },
        24 => RulesEvent::WaveCleared {
            tick: r.read_varint()?,
            wave: read_u32(r)?,
        },
        25 => RulesEvent::CoopEnded {
            tick: r.read_varint()?,
            won: r.read_bool()?,
            cause: match read_u8(r)? {
                0 => CoopCause::AllWavesCleared,
                1 => CoopCause::ObjectiveReached,
                2 => CoopCause::TeamWipe,
                3 => CoopCause::ObjectiveDestroyed,
                _ => return Err(DecodeError::Invalid("coop cause")),
            },
        },
        26 => RulesEvent::RacerCheckpoint {
            tick: r.read_varint()?,
            team: read_u8(r)?,
            slot: read_u8(r)?,
            checkpoint: read_u32(r)?,
            lap: read_u32(r)?,
        },
        27 => RulesEvent::RacerLap {
            tick: r.read_varint()?,
            team: read_u8(r)?,
            slot: read_u8(r)?,
            lap: read_u32(r)?,
            lap_ticks: r.read_varint()?,
            best: r.read_bool()?,
        },
        28 => RulesEvent::RacerMissedCheckpoint {
            tick: r.read_varint()?,
            team: read_u8(r)?,
            slot: read_u8(r)?,
            checkpoint: read_u32(r)?,
            expected: read_u32(r)?,
        },
        29 => RulesEvent::RacerWrongWay {
            tick: r.read_varint()?,
            team: read_u8(r)?,
            slot: read_u8(r)?,
        },
        30 => RulesEvent::RacerFinished {
            tick: r.read_varint()?,
            team: read_u8(r)?,
            slot: read_u8(r)?,
            place: read_u32(r)?,
        },
        31 => RulesEvent::RacerDnf {
            tick: r.read_varint()?,
            team: read_u8(r)?,
            slot: read_u8(r)?,
        },
        32 => RulesEvent::ZonePhase {
            tick: r.read_varint()?,
            phase: read_u32(r)?,
            stage: match read_u8(r)? {
                0 => ZoneStage::Idle,
                1 => ZoneStage::Waiting,
                2 => ZoneStage::Shrinking,
                3 => ZoneStage::Closed,
                _ => return Err(DecodeError::Invalid("zone stage")),
            },
            center: [r.read_f32()?, r.read_f32()?],
            radius: r.read_f32()?,
            target_center: [r.read_f32()?, r.read_f32()?],
            target_radius: r.read_f32()?,
        },
        34 => RulesEvent::TeamEliminated {
            tick: r.read_varint()?,
            team: read_u8(r)?,
            place: read_u8(r)?,
            cause: match read_u8(r)? {
                0 => OutCause::Eliminated,
                1 => OutCause::Surrendered,
                2 => OutCause::Destroyed,
                3 => OutCause::Host,
                _ => return Err(DecodeError::Invalid("out cause")),
            },
        },
        35 => {
            let tick = r.read_varint()?;
            let n = r.read_len(EVENT_MAX_TEAMS)?;
            let mut teams = Vec::with_capacity(n);
            for _ in 0..n {
                teams.push(read_u8(r)?);
            }
            RulesEvent::Victory {
                tick,
                teams,
                reason: reason(read_u8(r)?)?,
            }
        }
        36 => RulesEvent::SurrenderVote {
            tick: r.read_varint()?,
            team: read_u8(r)?,
            slot: read_u8(r)?,
            yes: r.read_bool()?,
        },
        37 => RulesEvent::SurrenderFailed {
            tick: r.read_varint()?,
            team: read_u8(r)?,
            until: r.read_varint()?,
        },
        38 => RulesEvent::RowSkipped {
            step: r.read_varint()?,
            index: read_u32(r)?,
            map_id: read_str(r)?,
            reason: match read_u8(r)? {
                0 => SkipReason::TooFewPlayers,
                1 => SkipReason::TooManyPlayers,
                2 => SkipReason::MissingContent,
                3 => SkipReason::Recent,
                _ => return Err(DecodeError::Invalid("skip reason")),
            },
        },
        _ => return Err(DecodeError::Invalid("match event kind")),
    })
}

/// Decodes one payload written by [`encode_match_event`]. A payload longer than `max_bytes` is
/// [`MatchEventDecodeError::Oversized`]; an unknown format or kind, a bad field, or bytes left
/// over after the event is [`MatchEventDecodeError::Malformed`]. Never panics.
pub fn decode_match_event(payload: &[u8], max_bytes: usize) -> Result<RulesEvent, MatchEventDecodeError> {
    if payload.len() > max_bytes {
        return Err(MatchEventDecodeError::Oversized);
    }
    let mut r = BitReader::new(payload);
    let event = decode_body(&mut r).map_err(|_| MatchEventDecodeError::Malformed)?;
    // The writer pads to a whole byte: more than 7 bits left is trailing data.
    if r.remaining_bits() > 7 {
        return Err(MatchEventDecodeError::Malformed);
    }
    Ok(event)
}

// ---- settings and counters ----------------------------------------------------------------------

/// Settings of [`MatchStateReplicationPlugin`] (same on every process of a session is not
/// required: each side reads its own).
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchStateSettings {
    /// Largest encoded event sent (server) or read (client), in bytes. A larger one is dropped
    /// and counted. Default [`DEFAULT_MAX_EVENT_BYTES`]; capped at [`MATCH_EVENT_WIRE_LIMIT`].
    pub max_event_bytes: usize,
    /// Server: rewrite the clock fields of [`MatchPhaseView`] at most every this many fixed ticks
    /// while nothing else changes (`0` and `1`: every tick). Clients extrapolate in between
    /// ([`MatchPhaseView::time_left_at`]; a pause, a phase change or any other field change still
    /// writes at once). Default [`DEFAULT_CLOCK_RESYNC_TICKS`].
    pub clock_resync_ticks: u32,
}

impl Default for MatchStateSettings {
    fn default() -> Self {
        Self {
            max_event_bytes: DEFAULT_MAX_EVENT_BYTES,
            clock_resync_ticks: DEFAULT_CLOCK_RESYNC_TICKS,
        }
    }
}

impl MatchStateSettings {
    fn event_cap(&self) -> usize {
        self.max_event_bytes.min(MATCH_EVENT_WIRE_LIMIT)
    }
}

/// What the match-state replication did (`rules.match_get` on a client shows it under
/// `replication`).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MatchStateCounters {
    /// Server: view components written (a write only happens on a change).
    pub views_written: u64,
    /// Server: events broadcast as [`MatchEventMsg`].
    pub events_sent: u64,
    /// Client: events decoded and written as [`MatchEvent`].
    pub events_received: u64,
    /// Events dropped for being longer than [`MatchStateSettings::max_event_bytes`] (server on
    /// send, client on receive).
    pub events_oversized: u64,
    /// Client: payloads dropped for an unknown format or kind, a bad field or trailing bytes.
    pub events_malformed: u64,
    /// Server: events of a kind this build cannot encode (a newer core variant), not sent.
    pub events_unencodable: u64,
}

// ---- plugin -------------------------------------------------------------------------------------

/// The `FixedUpdate` set the server writes the views and broadcasts events in: after
/// [`MatchRulesSet`], before `NetSet::Assign` (so the same tick's snapshot carries them).
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MatchStateReplicationSet;

/// Replicates the server's match state (see the module documentation). Add it on every process
/// of a session, after `NetPlugin`, at the same point of the replicated-component and message
/// registration order: it registers, in this order, [`TeamId`], [`MatchStateEntity`],
/// [`MatchPhaseView`], [`TeamScoresView`], [`ObjectiveViews`], [`PlayerScoreView`] and the
/// [`MatchEventMsg`] message, which moves the protocol hash once.
///
/// The server side runs when the app's `NetMode` is a server and it has [`MatchRules`] (stepped by
/// [`super::MatchSessionPlugin`] or by the game). The client side writes each received event as a
/// [`MatchEvent`]; install [`ReplicatedMatchRules`] as the client's game rules for the scoreboard.
pub struct MatchStateReplicationPlugin;

impl Plugin for MatchStateReplicationPlugin {
    fn build(&self, app: &mut App) {
        let world = app.world_mut();
        let has_team = world
            .get_resource::<ReplicationRegistry>()
            .and_then(|r| r.find(std::any::type_name::<TeamId>()))
            .is_some();
        if !has_team {
            register_replicated::<TeamId>(world, ReplicateOptions::default());
        }
        register_replicated::<MatchStateEntity>(world, ReplicateOptions::default());
        register_replicated::<MatchPhaseView>(world, ReplicateOptions::default());
        register_replicated::<TeamScoresView>(world, ReplicateOptions::default());
        register_replicated::<ObjectiveViews>(world, ReplicateOptions::default());
        register_replicated::<PlayerScoreView>(world, ReplicateOptions::default());
        app.add_net_message::<MatchEventMsg>()
            .add_message::<MatchEvent>()
            .init_resource::<MatchStateSettings>()
            .init_resource::<MatchStateCounters>()
            .init_resource::<MatchOverSeen>()
            .register_type::<MatchOverSeen>()
            .configure_sets(
                FixedUpdate,
                MatchStateReplicationSet
                    .after(MatchRulesSet)
                    .after(NetSet::Apply)
                    .before(NetSet::Assign),
            )
            .add_systems(
                FixedUpdate,
                (
                    (
                        ensure_match_state_entity,
                        note_match_over,
                        write_match_views,
                        write_player_score_views,
                        forward_match_events,
                    )
                        .chain()
                        .run_if(is_server),
                    receive_match_events.run_if(is_client),
                )
                    .in_set(MatchStateReplicationSet),
            );
    }
}

fn mode_of(world: &World) -> NetMode {
    world.get_resource::<NetMode>().copied().unwrap_or_default()
}

fn is_server(mode: Option<Res<NetMode>>) -> bool {
    mode.is_some_and(|m| m.is_server())
}

fn is_client(mode: Option<Res<NetMode>>) -> bool {
    mode.is_some_and(|m| !m.is_server() && m.is_client())
}

/// The net profile of the match-state entity: the built-in `game_state` profile.
pub const MATCH_STATE_PROFILE: &str = "game_state";

/// Server: spawns the match-state entity once [`MatchRules`] exists.
fn ensure_match_state_entity(world: &mut World) {
    if !world.contains_resource::<MatchRules>() {
        return;
    }
    let mut q = world.query_filtered::<(), With<MatchStateEntity>>();
    if q.iter(world).next().is_some() {
        return;
    }
    world.spawn((
        regia_scene::components::Name::new("MatchState"),
        MatchStateEntity,
        Replicated {
            transform: TransformReplication::None,
            ..Replicated::default()
        },
        // Game state every peer needs, with no position: the built-in `game_state` profile is
        // `sent_to_all` with world relevance, so interest management keeps it relevant and a
        // join snapshot (late join, reconnect) carries it (rules-match-state-replication-review
        // finding B). The project default profile is spatial, which drops an entity with no
        // position.
        regia_net::NetProfileRef(MATCH_STATE_PROFILE.to_owned()),
        NetPersistAcrossTravel,
        MatchPhaseView::default(),
        TeamScoresView::default(),
        ObjectiveViews::default(),
    ));
}

/// The phase view of `session` with its clock fields at server tick `net_tick`.
pub fn phase_view(session: &MatchSession, net_tick: u32) -> MatchPhaseView {
    let flow = &session.flow;
    let phases = &flow.phases;
    let board = &session.board;
    // The team the match end named (a forfeit or a condition winner may not lead on points),
    // else the sole score leader.
    let winner = if board.match_over {
        session.winner.unwrap_or_else(|| board_leader(session))
    } else {
        0
    };
    MatchPhaseView {
        phase: phase_code(phases.phase),
        round: phases.round,
        match_tick: flow.match_tick,
        time_left_ticks: phases.time_left(&flow.settings.phases),
        paused: flow.paused,
        pauser: flow.paused_by_seat.map(SeatView::from),
        paused_by: truncate(&flow.paused_by, MATCH_VIEW_MAX_PAUSE_NAME),
        between: flow.between,
        starts_at: flow.starts_at,
        flow_step: flow.step,
        match_over: board.match_over,
        winner,
        end_reason: board.end_reason.map_or(0, end_reason_code),
        net_tick,
    }
}

/// The sole leader of an ended board (`0` for a draw or a tie): the winner of a board whose
/// session names no team (`MatchSession::winner` is `None`, e.g. a session saved before it was
/// kept).
fn board_leader(session: &MatchSession) -> u8 {
    if session.board.end_reason == Some(EndReason::Draw) {
        return 0;
    }
    let mut best = (0u8, 0u32);
    let mut tied = false;
    for (team, &score) in session.board.team_scores.iter().enumerate().skip(1) {
        if score > best.1 {
            best = (u8::try_from(team).unwrap_or(u8::MAX), score);
            tied = false;
        } else if score == best.1 && score > 0 {
            tied = true;
        }
    }
    if tied { 0 } else { best.0 }
}

/// Server: the team the latest `MatchOver` event named, until the board is no longer over.
/// `write_match_views` stores it in `MatchSession::winner` (in the snapshotted `MatchRules`)
/// when the session has none; it is also reflected and registered, so it is in the world snapshot
/// itself (rules-match-state-replication-review finding A).
#[derive(Resource, Reflect, Debug, Clone, Copy, Default)]
#[reflect(Resource, Default)]
pub(crate) struct MatchOverSeen(Option<u8>);

/// Server: remembers the winner of this tick's `MatchOver` event.
fn note_match_over(mut reader: MessageReader<MatchEvent>, mut seen: ResMut<MatchOverSeen>) {
    for event in reader.read() {
        if let RulesEvent::MatchOver { team, .. } = event.0 {
            seen.0 = Some(team);
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

/// The team-score view of `session`: `max_teams + 1` entries at most.
pub fn team_scores_view(session: &MatchSession) -> TeamScoresView {
    let cap = (usize::from(session.board.settings.max_teams) + 1).min(MATCH_VIEW_MAX_TEAMS);
    let scores: Vec<u32> = session.board.team_scores.iter().take(cap).copied().collect();
    let round_wins: Vec<u32> = session.flow.phases.round_wins.iter().take(cap).copied().collect();
    let mut defs = session.teams.defs.clone();
    defs.sort_by_key(|d| d.id);
    defs.dedup_by_key(|d| d.id);
    let labels = defs
        .iter()
        .take(MATCH_VIEW_MAX_TEAMS)
        .map(|d| TeamLabelView {
            id: d.id,
            name: truncate(&d.name_key, MATCH_VIEW_MAX_LABEL),
            color: u32::from_be_bytes(d.color),
        })
        .collect();
    TeamScoresView {
        scores,
        round_wins,
        labels,
    }
}

/// A capture zone's view state and its bar in thousandths, in steps of 100 (ADR-8731).
/// `board_locked` is a host or logic-graph lock (`ObjectiveBoard::set_locked`).
fn zone_view(zone: Option<&regia_rules::zones::Zone>, board_locked: bool) -> (ObjectiveViewState, u16) {
    let Some(zone) = zone else {
        return (
            ObjectiveViewState::Zone {
                owner: 0,
                capturing: 0,
                locked: board_locked,
            },
            0,
        );
    };
    // In `u128`, like the dropped-flag timer: `capture_ticks` and a saved `progress` may sit near
    // `u64::MAX`, where `progress * 10` overflows `u64` (rules-review-fixes-b-verification
    // finding 1). A full zone still reads as a full bar and 3/10 still reads as 300.
    let full = zone.settings.effective_capture_ticks().max(1);
    let tenths = u128::from(zone.progress.min(full)) * 10 / u128::from(full);
    let tenths = u16::try_from(tenths).unwrap_or(10).min(10);
    (
        ObjectiveViewState::Zone {
            owner: zone.owner,
            capturing: zone.capturing_team,
            locked: zone.locked || board_locked,
        },
        tenths * (CAPTURE_PROGRESS_FULL / 10),
    )
}

/// The objective views of `session`: `max_objectives` at most.
pub fn objective_views(session: &MatchSession) -> ObjectiveViews {
    let board = &session.board;
    let cap = usize::from(board.settings.max_objectives).min(MATCH_VIEW_MAX_OBJECTIVES);
    let now = session.flow.match_tick;
    let items = board
        .objectives
        .iter()
        .take(cap)
        .map(|o| {
            let (state, progress) = match o.kind {
                ObjectiveKind::Hill => (ObjectiveViewState::Hill, 0),
                ObjectiveKind::Zone => zone_view(o.zone.as_ref(), board.is_locked(o.entity_id)),
                ObjectiveKind::Flag => match (o.carrier_id, o.at_base) {
                    (Some(carrier), _) => {
                        let seat = session
                            .ledgers
                            .rows
                            .iter()
                            .find(|row| row.unit == Some(carrier))
                            .map(|row| SeatView::from(row.seat()));
                        (ObjectiveViewState::Carried { carrier, seat }, 0)
                    }
                    (None, true) => (ObjectiveViewState::AtBase, 0),
                    (None, false) => {
                        let progress = match (o.dropped_tick, board.settings.flag_return_ticks) {
                            (Some(at), Some(ticks)) if ticks > 0 => {
                                let run = now.saturating_sub(at).min(ticks);
                                // In u128: `run * 10` overflows u64 once both the timer and the
                                // elapsed ticks pass `u64::MAX / 10` (finding C). `run <= ticks`,
                                // so the tenths are 0..=10.
                                let tenths = u128::from(run) * 10 / u128::from(ticks);
                                u16::try_from(tenths).unwrap_or(10).min(10) * (CAPTURE_PROGRESS_FULL / 10)
                            }
                            _ => 0,
                        };
                        (ObjectiveViewState::Dropped, progress)
                    }
                },
                // A kind this build does not know shows as a hill: a fixed volume, no bar.
                _ => (ObjectiveViewState::Hill, 0),
            };
            ObjectiveView {
                id: o.entity_id,
                kind: match o.kind {
                    ObjectiveKind::Flag => 0,
                    ObjectiveKind::Zone => 2,
                    _ => 1,
                },
                team: o.team_id,
                state,
                position: if matches!(state, ObjectiveViewState::Carried { .. }) {
                    Vec3::from(o.base_position)
                } else {
                    Vec3::from(o.position)
                },
                capture_progress: progress,
            }
        })
        .collect();
    ObjectiveViews { items }
}

/// Server: rewrites the views from the session, only where they changed.
fn write_match_views(world: &mut World) {
    let _span = tracing::trace_span!(target: "regia_framework", "match_state_views").entered();
    let net_tick = world.get_resource::<NetNode>().map_or(0, |n| n.tick.0);
    let resync = world
        .get_resource::<MatchStateSettings>()
        .map_or(DEFAULT_CLOCK_RESYNC_TICKS, |s| s.clock_resync_ticks.max(1));
    let seen = world.get_resource::<MatchOverSeen>().and_then(|s| s.0);
    // The named winner is session state: keep the one this server saw in `MatchSession::winner`,
    // which snapshots and hot reloads carry (`MatchOverSeen` alone is a one-shot memory), and
    // drop it once the board plays again. Written only on a change, so change detection stays
    // quiet.
    if let Some(mut rules) = world.get_resource_mut::<MatchRules>() {
        let over = rules.session.board.match_over;
        let want = if over { rules.session.winner.or(seen) } else { None };
        if rules.session.winner != want {
            rules.session.winner = want;
        }
    }
    let Some(rules) = world.get_resource::<MatchRules>() else {
        return;
    };
    let phase = phase_view(&rules.session, net_tick);
    let over = rules.session.board.match_over;
    let teams = team_scores_view(&rules.session);
    let objectives = objective_views(&rules.session);
    let mut q = world.query_filtered::<(
        &mut MatchPhaseView,
        &mut TeamScoresView,
        &mut ObjectiveViews,
    ), With<MatchStateEntity>>();
    let mut written = 0u64;
    for (mut p, mut t, mut o) in q.iter_mut(world) {
        let due = net_tick.saturating_sub(p.net_tick) >= resync;
        if *p != phase && (due || !p.same_except_clock(&phase)) {
            *p = phase.clone();
            written += 1;
        }
        if *t != teams {
            *t = teams.clone();
            written += 1;
        }
        if *o != objectives {
            *o = objectives.clone();
            written += 1;
        }
    }
    if !over && let Some(mut seen) = world.get_resource_mut::<MatchOverSeen>() {
        seen.0 = None;
    }
    if written > 0 {
        world.resource_mut::<MatchStateCounters>().views_written += written;
    }
}

/// Server: one [`PlayerScoreView`] per replicated entity with [`TeamId`] and [`LedgerSlot`], from
/// its seat's ledger row; removed when the seat has no row.
fn write_player_score_views(world: &mut World) {
    if !world.contains_resource::<MatchRules>() {
        return;
    }
    let mut q =
        world.query_filtered::<(Entity, &TeamId, &LedgerSlot, Option<&PlayerScoreView>), With<Replicated>>();
    let world_ref: &World = world;
    let ledgers = &world_ref.resource::<MatchRules>().session.ledgers;
    let mut changes: Vec<(Entity, Option<PlayerScoreView>)> = Vec::new();
    for (entity, team, slot, current) in q.iter(world_ref) {
        let row = if team.0 == 0 {
            None
        } else {
            ledgers.row(SeatId::new(team.0, slot.0))
        };
        let view = row.map(|row| PlayerScoreView {
            team: row.team,
            slot: row.slot,
            row_id: row.id,
            kills: row.kills,
            deaths: row.deaths,
            assists: row.assists,
            objective_points: row.objective_points,
            score: row.score,
        });
        if view.as_ref() != current {
            changes.push((entity, view));
        }
    }
    let written = changes.len() as u64;
    for (entity, view) in changes {
        let Ok(mut e) = world.get_entity_mut(entity) else {
            continue;
        };
        match view {
            Some(view) => {
                e.insert(view);
            }
            None => {
                e.remove::<PlayerScoreView>();
            }
        }
    }
    if written > 0 {
        world.resource_mut::<MatchStateCounters>().views_written += written;
    }
}

/// Server: broadcasts each [`MatchEvent`] of this tick as a [`MatchEventMsg`].
fn forward_match_events(
    mut reader: MessageReader<MatchEvent>,
    mut sender: NetSender,
    settings: Res<MatchStateSettings>,
    mut counters: ResMut<MatchStateCounters>,
) {
    let cap = settings.event_cap();
    for event in reader.read() {
        // Per-unit zone damage stays on the server (see `encode_match_event`).
        if matches!(event.0, RulesEvent::ZoneDamage { .. }) {
            continue;
        }
        let Some(payload) = encode_match_event(&event.0) else {
            counters.events_unencodable += 1;
            continue;
        };
        if payload.len() > cap {
            counters.events_oversized += 1;
            continue;
        }
        if sender.broadcast_session(MatchEventMsg { payload }).is_ok() {
            counters.events_sent += 1;
        }
    }
}

/// Client: decodes each received [`MatchEventMsg`] into a [`MatchEvent`], in delivery order.
fn receive_match_events(world: &mut World) {
    let payloads: Vec<Vec<u8>> = match world.get_resource::<NetMessages<MatchEventMsg>>() {
        Some(messages) if !messages.is_empty() => messages.iter().map(|m| m.msg.payload.clone()).collect(),
        _ => return,
    };
    let cap = world
        .get_resource::<MatchStateSettings>()
        .copied()
        .unwrap_or_default()
        .event_cap();
    let mut counters = world
        .get_resource::<MatchStateCounters>()
        .copied()
        .unwrap_or_default();
    for payload in payloads {
        match decode_match_event(&payload, cap) {
            Ok(event) => {
                counters.events_received += 1;
                world.write_message(MatchEvent(event));
            }
            Err(MatchEventDecodeError::Oversized) => counters.events_oversized += 1,
            Err(MatchEventDecodeError::Malformed) => counters.events_malformed += 1,
        }
    }
    world.insert_resource(counters);
}

// ---- the client's rules -------------------------------------------------------------------------

/// The views of the match-state entity, if the world has one.
pub fn match_views(world: &mut World) -> Option<(MatchPhaseView, TeamScoresView, ObjectiveViews)> {
    let mut q =
        world.query_filtered::<(&MatchPhaseView, &TeamScoresView, &ObjectiveViews), With<MatchStateEntity>>();
    q.iter(world)
        .next()
        .map(|(p, t, o)| (p.clone(), t.clone(), o.clone()))
}

/// Where an objective is now, as a client sees it: a carried flag follows the replicated entity
/// whose [`PlayerScoreView`] names the carrier's seat (its `Transform`); any other objective, or a
/// carrier this client cannot find, is [`ObjectiveView::position`].
///
/// When several entities carry a view of that seat, the one picked does not depend on spawn or
/// query order: the lowest [`regia_net::NetId`] (server-assigned, never reused), then the lowest
/// translation (x, then y, then z, by total order) for entities without one
/// (rules-match-state-replication-review finding D).
pub fn objective_position(world: &mut World, view: &ObjectiveView) -> Vec3 {
    let ObjectiveViewState::Carried { seat: Some(seat), .. } = view.state else {
        return view.position;
    };
    let mut q = world.query::<(
        &PlayerScoreView,
        &regia_scene::Transform,
        Option<&regia_net::NetId>,
    )>();
    q.iter(world)
        .filter(|(v, ..)| v.team == seat.team && v.slot == seat.slot)
        .map(|(_, t, id)| (id.map_or(u64::MAX, |id| u64::from(id.0)), t.translation))
        .min_by(|(la, lt), (ra, rt)| {
            la.cmp(ra)
                .then(lt.x.total_cmp(&rt.x))
                .then(lt.y.total_cmp(&rt.y))
                .then(lt.z.total_cmp(&rt.z))
        })
        .map_or(view.position, |(_, translation)| translation)
}

/// Every [`PlayerScoreView`] in the world with whether it is this machine's player
/// ([`NetOwnedLocally`]), in score-sheet order (score, then kills, descending; then team and slot).
pub fn player_score_views(world: &mut World) -> Vec<(PlayerScoreView, bool)> {
    let mut q = world.query::<(&PlayerScoreView, Has<NetOwnedLocally>)>();
    let mut rows: Vec<(PlayerScoreView, bool)> = q.iter(world).map(|(v, mine)| (v.clone(), mine)).collect();
    // After team, slot and row id, the remaining fields and the local flag make the order total:
    // two entities stamped with one seat show equal views, and the order of equal rows must not
    // follow spawn or query order (rules-match-state-replication-review finding D).
    rows.sort_by(|(l, lm), (r, rm)| {
        r.score
            .cmp(&l.score)
            .then(r.kills.cmp(&l.kills))
            .then(l.team.cmp(&r.team))
            .then(l.slot.cmp(&r.slot))
            .then(l.row_id.cmp(&r.row_id))
            .then(l.deaths.cmp(&r.deaths))
            .then(l.assists.cmp(&r.assists))
            .then(l.objective_points.cmp(&r.objective_points))
            .then(lm.cmp(rm))
    });
    rows
}

/// The standings from the replicated views, in the shape of
/// [`super::match_session_score_sheet`]: one row per [`PlayerScoreView`] (score, kills, deaths,
/// assists, objective), each team heading followed by the team's score.
pub fn replicated_score_sheet(world: &mut World) -> Option<ScoreSheet> {
    let (_, teams, _) = match_views(world)?;
    let rows = player_score_views(world)
        .into_iter()
        .map(|(v, local)| ScoreSheetRow {
            id: v.row_id,
            name: seat_label(SeatId::new(v.team, v.slot)),
            team: Some(v.team),
            local,
            values: vec![
                ScoreCell::Int(v.score),
                ScoreCell::Int(i64::from(v.kills)),
                ScoreCell::Int(i64::from(v.deaths)),
                ScoreCell::Int(i64::from(v.assists)),
                ScoreCell::Int(i64::from(v.objective_points)),
            ],
        })
        .collect();
    let headings = teams
        .labels
        .iter()
        .map(|label| {
            let [r, g, b, a] = label.color.to_be_bytes();
            let name = if label.name.starts_with("loc:") {
                label.name.clone()
            } else {
                let base = if label.name.is_empty() {
                    format!("Team {}", label.id)
                } else {
                    label.name.clone()
                };
                format!("{base}: {}", teams.score(label.id))
            };
            ScoreSheetTeam {
                id: label.id,
                name,
                color: Some([
                    f32::from(r) / 255.0,
                    f32::from(g) / 255.0,
                    f32::from(b) / 255.0,
                    f32::from(a) / 255.0,
                ]),
            }
        })
        .collect();
    Some(ScoreSheet {
        columns: vec![
            ScoreSheetColumn::new("score", "Score"),
            ScoreSheetColumn::new("kills", "Kills"),
            ScoreSheetColumn::new("deaths", "Deaths"),
            ScoreSheetColumn::new("assists", "Assists"),
            ScoreSheetColumn::new("objective", "Objective"),
        ],
        rows,
        teams: headings,
    })
}

/// A client's [`GameRules`]: no rules logic, only the replicated views. `scoreboard` is
/// [`replicated_score_sheet`]; `is_match_over` reads [`MatchPhaseView`]. Joins spawn nothing (the
/// server spawns).
#[derive(Debug, Default, Clone, Copy)]
pub struct ReplicatedMatchRules;

impl GameRules for ReplicatedMatchRules {
    fn on_player_joined(&mut self, _ctx: &mut RulesCtx, _player: Entity) -> Option<SpawnRequest> {
        None
    }

    fn is_match_over(&self, world: &World) -> bool {
        world
            .iter_entities()
            .filter(|e| e.contains::<MatchStateEntity>())
            .filter_map(|e| e.get::<MatchPhaseView>())
            .any(|v| v.match_over || v.phase() == MatchPhase::PostMatch)
    }

    fn scoreboard(&mut self, ctx: &mut RulesCtx) -> Option<ScoreSheet> {
        replicated_score_sheet(ctx.world)
    }
}

/// The net mode of the app is a client that only shows replicated state (helper for hosts that
/// pick between [`super::MatchSessionRules`] and [`ReplicatedMatchRules`]).
pub fn shows_replicated_match(world: &World) -> bool {
    let mode = mode_of(world);
    !mode.is_server() && mode.is_client()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_event() -> Vec<RulesEvent> {
        vec![
            RulesEvent::FlagTaken {
                tick: 5,
                flag: 900,
                unit: 3,
                team: 1,
                flag_team: 2,
            },
            RulesEvent::FlagDropped {
                tick: 6,
                flag: 900,
                unit: 3,
                flag_team: 2,
                position: [1.5, -2.0, 3.25],
            },
            RulesEvent::FlagReturned {
                tick: 7,
                flag: 900,
                unit: 4,
                flag_team: 2,
            },
            RulesEvent::FlagCaptured {
                tick: 8,
                flag: 901,
                unit: 3,
                team: 1,
                flag_team: 2,
                score: 2,
            },
            RulesEvent::HillPoint {
                tick: 9,
                team: 2,
                score: 70_000,
            },
            RulesEvent::MatchOver {
                tick: 10,
                team: 1,
                reason: EndReason::CaptureLimitReached,
            },
            RulesEvent::Paused {
                tick: 11,
                by: "Ünïcode host".to_owned(),
            },
            RulesEvent::Resumed { tick: 12 },
            RulesEvent::LaunchScheduled { at_step: 13 },
            RulesEvent::LaunchCancelled { step: 14 },
            RulesEvent::Launched {
                step: 15,
                index: Some(2),
                map_id: "switchbacks".to_owned(),
                mode: "ctf".to_owned(),
            },
            RulesEvent::Launched {
                step: 16,
                index: None,
                map_id: String::new(),
                mode: String::new(),
            },
            RulesEvent::SeatLeft {
                step: 17,
                team: 2,
                slot: 3,
            },
            RulesEvent::PhaseChanged {
                step: 18,
                phase: MatchPhase::Overtime,
                round: 4,
            },
            RulesEvent::RoundOver {
                tick: 19,
                round: 4,
                team: 0,
                reason: EndReason::Draw,
            },
            RulesEvent::FlagAutoReturned {
                tick: 20,
                flag: 900,
                flag_team: 1,
                cause: AutoReturnCause::DropCheck,
            },
            RulesEvent::PlayerScored {
                tick: 21,
                team: 1,
                slot: 0,
                reason: ScoreReason::Assist,
                score: -3,
            },
            RulesEvent::PlayerScored {
                tick: 22,
                team: 2,
                slot: 1,
                reason: ScoreReason::Scripted,
                score: 40,
            },
            RulesEvent::MatchOver {
                tick: 23,
                team: 2,
                reason: EndReason::ZonesCaptured,
            },
            RulesEvent::RoundOver {
                tick: 24,
                round: 1,
                team: 3,
                reason: EndReason::Surrendered,
            },
        ]
        .into_iter()
        // ADR-8731: the events the v0.7 lanes added (zone damage is never sent).
        .chain(
            super::super::match_events::later_events()
                .into_iter()
                .filter(|e| !matches!(e, RulesEvent::ZoneDamage { .. })),
        )
        .collect()
    }

    /// ADR-8731: every end reason has its own wire code, so a match a capture zone, an
    /// elimination or a surrender ended reaches the clients (code 255 would not decode).
    #[test]
    fn every_end_reason_has_a_wire_code() {
        let reasons = [
            EndReason::CaptureLimitReached,
            EndReason::HillScoreLimitReached,
            EndReason::Ended,
            EndReason::TimeLimit,
            EndReason::Draw,
            EndReason::Forfeit,
            EndReason::RoundsWon,
            EndReason::ZoneScoreLimitReached,
            EndReason::ZonesCaptured,
            EndReason::LastStanding,
            EndReason::Destroyed,
            EndReason::Held,
            EndReason::ScoreReached,
            EndReason::Surrendered,
        ];
        let mut codes: Vec<u8> = reasons.iter().map(|r| end_reason_code(*r)).collect();
        for (reason, code) in reasons.iter().zip(&codes) {
            assert_ne!(*code, u8::MAX, "{reason:?}");
            assert_eq!(end_reason_from_code(*code), Some(*reason));
        }
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), reasons.len());
    }

    /// Zone damage is the server's damage instruction: it has no wire form, and kind 33 (kept
    /// free for it) does not decode.
    #[test]
    fn zone_damage_is_not_sent() {
        let damage = RulesEvent::ZoneDamage {
            tick: 1,
            unit: 2,
            amount: 3,
        };
        assert!(encode_match_event(&damage).is_none());
        let payload = [MATCH_EVENT_FORMAT, 33, 1, 2, 3];
        assert_eq!(
            decode_match_event(&payload, DEFAULT_MAX_EVENT_BYTES),
            Err(MatchEventDecodeError::Malformed)
        );
    }

    /// ADR-8731: a capture zone shows on the wire as a zone, with its owner, the team filling its
    /// bar, its lock and the bar in steps of 100 thousandths.
    #[test]
    fn a_capture_zone_has_a_zone_view_with_its_bar() {
        use regia_rules::objective::{Contestant, HillVolume};
        use regia_rules::session::SessionSettings;
        use regia_rules::zones::ZoneSettings;
        let mut session = MatchSession::new(SessionSettings::default());
        let settings = ZoneSettings {
            capture_ticks: 10,
            ..ZoneSettings::new(HillVolume {
                min: [-5.0, -1.0, -5.0],
                max: [5.0, 9.0, 5.0],
            })
        };
        session.board.install_zone(5, settings).expect("zone");
        let inside = [Contestant {
            id: 1,
            team: 1,
            position: [0.0; 3],
            alive: true,
        }];
        for tick in 1..=3 {
            session.board.zones_tick(tick, &inside);
        }
        let views = objective_views(&session);
        let view = views.items.iter().find(|v| v.id == 5).expect("zone view");
        assert_eq!(view.kind(), ObjectiveKind::Zone);
        assert_eq!(
            view.state,
            ObjectiveViewState::Zone {
                owner: 0,
                capturing: 1,
                locked: false,
            }
        );
        assert_eq!((view.team, view.capture_progress), (0, 300));
    }

    #[test]
    fn every_event_kind_round_trips_under_the_default_cap() {
        for event in every_event() {
            let bytes = encode_match_event(&event).expect("known kind");
            assert!(
                bytes.len() <= DEFAULT_MAX_EVENT_BYTES,
                "{event:?}: {}",
                bytes.len()
            );
            assert_eq!(decode_match_event(&bytes, DEFAULT_MAX_EVENT_BYTES), Ok(event));
        }
    }

    #[test]
    fn long_strings_are_cut_on_a_char_boundary() {
        let by = "é".repeat(200);
        let bytes = encode_match_event(&RulesEvent::Paused { tick: 1, by }).expect("encodes");
        match decode_match_event(&bytes, MATCH_EVENT_WIRE_LIMIT) {
            Ok(RulesEvent::Paused { by, .. }) => assert_eq!(by.len(), EVENT_MAX_STR),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bad_payloads_are_errors_not_panics() {
        assert_eq!(
            decode_match_event(&[], 256),
            Err(MatchEventDecodeError::Malformed)
        );
        assert_eq!(
            decode_match_event(&[MATCH_EVENT_FORMAT, 200], 256),
            Err(MatchEventDecodeError::Malformed),
            "unknown kind"
        );
        assert_eq!(
            decode_match_event(&[9, 1], 256),
            Err(MatchEventDecodeError::Malformed),
            "unknown format"
        );
        assert_eq!(
            decode_match_event(&[0; 300], 256),
            Err(MatchEventDecodeError::Oversized)
        );
        let mut good = encode_match_event(&RulesEvent::Resumed { tick: 3 }).expect("encodes");
        good.push(0);
        assert_eq!(
            decode_match_event(&good, 256),
            Err(MatchEventDecodeError::Malformed),
            "trailing byte"
        );
        // Seeded bytes: every outcome is a value, never a panic.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..10_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let len = (state % 64) as usize;
            let bytes: Vec<u8> = (0..len)
                .map(|i| (state.rotate_left(i as u32 * 7) & 0xff) as u8)
                .collect();
            let _ = decode_match_event(&bytes, 256);
        }
    }

    #[test]
    fn phase_view_clock_extrapolates_while_live_only() {
        let view = MatchPhaseView {
            phase: phase_code(MatchPhase::Live),
            match_tick: 100,
            time_left_ticks: Some(50),
            net_tick: 1000,
            ..MatchPhaseView::default()
        };
        assert_eq!(view.time_left_at(1010), Some(40));
        assert_eq!(view.time_left_at(2000), Some(0));
        assert_eq!(view.match_tick_at(1010), 110);
        let paused = MatchPhaseView {
            paused: true,
            ..view.clone()
        };
        assert_eq!(paused.time_left_at(1010), Some(50));
        assert_eq!(paused.match_tick_at(1010), 100);
        let warmup = MatchPhaseView {
            phase: phase_code(MatchPhase::Warmup),
            ..view
        };
        assert_eq!(warmup.time_left_at(1010), Some(50));
    }

    #[test]
    fn an_ended_board_names_only_a_sole_leader() {
        let mut session = MatchSession::default();
        session.board.match_over = true;
        session.board.team_scores = vec![0, 2, 2, 0, 0];
        session.board.end_reason = Some(EndReason::TimeLimit);
        assert_eq!(phase_view(&session, 0).winner, 0, "a tie names nobody");
        session.board.team_scores[2] = 3;
        assert_eq!(phase_view(&session, 0).winner, 2);
        session.board.end_reason = Some(EndReason::Draw);
        assert_eq!(phase_view(&session, 0).winner, 0, "a draw names nobody");
        session.board.match_over = false;
        session.board.end_reason = None;
        assert_eq!(phase_view(&session, 0).winner, 0, "a running match has no winner");
    }

    /// Team 1 leads 5–1. The host ended the match for team 2 (`EndReason::Forfeit`). The view
    /// names team 2 only while [`MatchOverSeen`] still holds that event. A snapshot or a hot
    /// reload drops the resource (it is not reflected), and the next write names the score leader.
    fn forfeit_world(seen: Option<u8>) -> World {
        let mut world = World::new();
        world.insert_resource(MatchStateCounters::default());
        world.insert_resource(MatchOverSeen(seen));
        let mut session = MatchSession::default();
        session.board.team_scores = vec![0, 5, 1];
        session.board.match_over = true;
        session.board.end_reason = Some(EndReason::Forfeit);
        world.insert_resource(MatchRules { session });
        world.spawn((
            MatchStateEntity,
            MatchPhaseView::default(),
            TeamScoresView::default(),
            ObjectiveViews::default(),
        ));
        world
    }

    #[test]
    fn a_named_forfeit_winner_survives_losing_match_over_seen() {
        let mut world = forfeit_world(Some(2));
        write_match_views(&mut world);
        let (phase, _, _) = match_views(&mut world).expect("view");
        assert_eq!(phase.winner, 2, "the MatchOver event names team 2");
        // What a world snapshot does: MatchOverSeen is not reflected, so it comes back empty.
        world.insert_resource(MatchOverSeen::default());
        write_match_views(&mut world);
        let (phase, _, _) = match_views(&mut world).expect("view");
        assert_eq!(
            phase.winner, 2,
            "team 2 won by forfeit while team 1 leads 5-1; a restore must keep team 2"
        );
    }

    #[test]
    fn match_over_seen_is_in_the_world_snapshot() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, crate::rules::MatchRulesPlugin));
        app.insert_resource(MatchOverSeen(Some(2)));
        let mut snapshot = crate::state_hash::world_snapshot(app.world());
        assert!(
            crate::state_hash::insert_resource_text(app.world(), &mut snapshot, "MatchRules"),
            "the registry is present and MatchRules is reflected"
        );
        assert!(
            crate::state_hash::insert_resource_text(app.world(), &mut snapshot, "MatchOverSeen"),
            "MatchOverSeen holds the named winner and must be in the snapshot"
        );
    }

    /// A rules file can set `flag_return_ticks` to `u64::MAX`. Once the match tick is also past
    /// `u64::MAX / 10` (a saved session, or that timer elapsing), `run * 10` overflows.
    #[test]
    fn a_huge_flag_return_timer_does_not_overflow() {
        let mut session = MatchSession::default();
        session.board.settings.flag_return_ticks = Some(u64::MAX);
        session.flow.match_tick = u64::MAX;
        session.board.objectives.push(regia_rules::objective::Objective {
            entity_id: 1,
            kind: ObjectiveKind::Flag,
            team_id: 1,
            carrier_id: None,
            at_base: false,
            base_position: [0.0, 0.0, 0.0],
            position: [1.0, 0.0, 0.0],
            volume: None,
            dropped_tick: Some(0),
            // Capture zones (merged after the review's head) added this field; a flag has none.
            zone: None,
        });
        let views = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| objective_views(&session)));
        let views = views.expect("dropped-flag progress must not panic");
        let progress = views
            .items
            .first()
            .map(|item| item.capture_progress)
            .unwrap_or(u16::MAX);
        assert!(
            progress <= CAPTURE_PROGRESS_FULL,
            "progress {progress} is outside 0..={CAPTURE_PROGRESS_FULL}"
        );
    }

    /// A rules file can set a zone's `capture_ticks` to `u64::MAX`. A saved zone whose progress is
    /// also past `u64::MAX / 10` makes `progress * 10` overflow in the bar: the same multiply the
    /// dropped-flag timer used to do, in `zone_view` next to that fix. A full zone reads as a full
    /// bar.
    #[test]
    fn a_huge_capture_zone_bar_does_not_overflow() {
        use regia_rules::objective::HillVolume;
        use regia_rules::session::SessionSettings;
        use regia_rules::zones::ZoneSettings;
        let mut session = MatchSession::new(SessionSettings::default());
        let settings = ZoneSettings {
            capture_ticks: u64::MAX,
            ..ZoneSettings::new(HillVolume {
                min: [0.0; 3],
                max: [1.0; 3],
            })
        };
        session.board.install_zone(1, settings).expect("zone");
        let zone = session
            .board
            .objectives
            .iter_mut()
            .find(|objective| objective.entity_id == 1)
            .and_then(|objective| objective.zone.as_mut())
            .expect("installed zone");
        zone.progress = u64::MAX;
        let views = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| objective_views(&session)));
        let views = views.expect("zone bar must not panic");
        let progress = views
            .items
            .iter()
            .find(|item| item.id == 1)
            .map(|item| item.capture_progress)
            .unwrap_or(u16::MAX);
        assert_eq!(
            progress, CAPTURE_PROGRESS_FULL,
            "a zone whose progress has reached its capture time reads as a full bar, got {progress}"
        );
    }

    /// The match-state entity is game state. Join snapshots and the relevance pipeline keep an
    /// entity with no transform only when its profile is `sent_to_all`.
    #[test]
    fn match_state_entity_is_sent_to_every_peer() {
        let mut world = World::new();
        world.insert_resource(MatchRules::default());
        ensure_match_state_entity(&mut world);
        regia_net::install_net_interest(&mut world);
        let mut query = world.query_filtered::<Entity, With<MatchStateEntity>>();
        let entity = query.iter(&world).next().expect("match state entity");
        let has_transform = world
            .get_entity(entity)
            .is_ok_and(|e| e.contains::<regia_scene::Transform>());
        let named = world
            .get_entity(entity)
            .ok()
            .and_then(|e| e.get::<regia_net::NetProfileRef>().map(|r| r.0.clone()));
        let key = named.unwrap_or_else(|| {
            world
                .resource::<regia_net::NetProfiles>()
                .default_key()
                .to_string()
        });
        let profiles = world.resource::<regia_net::NetProfiles>();
        let profile = profiles.get(&key).expect("resolved profile");
        assert!(
            profile.sent_to_all,
            "profile `{key}` sent_to_all={} relevance={:?}; transform present={has_transform}. \
             A spatial profile with no position is not relevant (`no position`), and \
             join records omit an entity that is not sent_to_all",
            profile.sent_to_all, profile.relevance
        );
    }

    /// Two entities with the same seat. The carried flag's client position must not follow
    /// which one was spawned first.
    #[test]
    fn carried_flag_position_ignores_duplicate_seat_spawn_order() {
        let view = ObjectiveView {
            state: ObjectiveViewState::Carried {
                carrier: 1,
                seat: Some(SeatView { team: 1, slot: 0 }),
            },
            position: Vec3::ZERO,
            ..ObjectiveView::default()
        };
        let score = PlayerScoreView {
            team: 1,
            slot: 0,
            row_id: 7,
            ..PlayerScoreView::default()
        };
        let near = Vec3::new(1.0, 0.0, 0.0);
        let far = Vec3::new(50.0, 0.0, 0.0);
        let spawn = |first: Vec3, second: Vec3| {
            let mut world = World::new();
            world.spawn((score.clone(), regia_scene::Transform::from_translation(first)));
            world.spawn((score.clone(), regia_scene::Transform::from_translation(second)));
            world
        };
        let mut first_near = spawn(near, far);
        let mut first_far = spawn(far, near);
        let near_first = objective_position(&mut first_near, &view);
        let far_first = objective_position(&mut first_far, &view);
        assert!(
            near_first == near || near_first == far,
            "the seat query missed both entities: {near_first:?}"
        );
        assert_eq!(
            near_first, far_first,
            "carried flag position follows spawn order: {near_first:?} vs {far_first:?}"
        );
    }
}
