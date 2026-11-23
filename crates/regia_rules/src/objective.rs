// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/modes/mod.rs, commit d6ed37677), ADR-8012.
// Mark: adapt (lines 68-207: the kind tags, the touch radius and limit defaults, the hill volume, the
// objective record and the XZ distance). Named issues: an objective owns its position (the game's
// entity store is no longer reached into; the host mirrors it onto its entity), the four-team range
// and the objective cap are settings, the kind tag is an enum with the same strings, and scores,
// match-over and events live on one board instead of the game's runtime state.

//! Objective entities: flags and hills, the units that touch them, and the board that keeps the
//! match's objectives, team scores, the match-over flag and the events of the last steps.
//!
//! Everything here is plain data stepped in a fixed order with `+ - *` and comparisons only, so a
//! lockstep match computes the same bits on every peer (ADR-8012 A-4; `cargo xtask det-scan` scans
//! this directory).

use serde::{Deserialize, Serialize};

use super::mode::MatchMode;
use super::phases::MatchPhase;

/// Objective kind tag for a capture-the-flag flag. It is a mode tag, not an entity or asset name.
pub const KIND_FLAG: &str = "flag";

/// Objective kind tag for a king-of-the-hill hill. It is a mode tag, not an entity or asset name.
pub const KIND_HILL: &str = "hill";

/// Objective kind tag for a capture zone (domination, hardpoint, attack/defend). It is a mode tag,
/// not an entity or asset name.
pub const KIND_ZONE: &str = "zone";

/// Neutral name for the entity a host spawns for an objective (not a kind tag).
pub const OBJECTIVE_ENTITY_NAME: &str = "objective";

/// Default touch (pickup, return, capture) radius of a flag, in world units across XZ.
pub const DEFAULT_FLAG_TOUCH_RADIUS: f32 = 3.0;

/// Default captures to win capture the flag.
pub const DEFAULT_CAPTURE_LIMIT: u32 = 3;

/// Default hill score to win king of the hill (one point per accrual period).
pub const DEFAULT_HILL_SCORE_LIMIT: u32 = 180;

/// Default number of fighting teams (`1..=4`); team 0 is neutral.
pub const DEFAULT_MAX_TEAMS: u8 = 4;

/// Default objective cap of a match.
pub const DEFAULT_MAX_OBJECTIVES: u16 = 8;

/// Default pending-event cap. A host that drains every step never reaches it, which is S-14's
/// behavior. A host that steps `ctf_tick` or `koth_tick` and does not drain drops the oldest events
/// past this cap and counts them. `0` in stored settings means this default.
pub const DEFAULT_MAX_PENDING_EVENTS: u32 = 4096;

fn default_max_pending_events() -> u32 {
    DEFAULT_MAX_PENDING_EVENTS
}

/// The cap in force for `stored`: `0` is [`DEFAULT_MAX_PENDING_EVENTS`].
pub(crate) fn pending_event_cap(stored: u32) -> usize {
    let cap = if stored == 0 {
        DEFAULT_MAX_PENDING_EVENTS
    } else {
        stored
    };
    usize::try_from(cap).unwrap_or(usize::MAX)
}

/// Push `event`, dropping the oldest pending events when the cap is full. The drop count is never
/// silent: callers keep it on `dropped`.
pub(crate) fn push_capped(
    events: &mut Vec<RulesEvent>,
    dropped: &mut u64,
    stored_cap: u32,
    event: RulesEvent,
) {
    let cap = pending_event_cap(stored_cap);
    if cap == 0 {
        *dropped = dropped.saturating_add(1);
        return;
    }
    if events.len() >= cap {
        let extra = events.len() + 1 - cap;
        events.drain(..extra);
        *dropped = dropped.saturating_add(u64::try_from(extra).unwrap_or(u64::MAX));
    }
    events.push(event);
}

/// What an objective is.
///
/// Non-exhaustive since the capture zones (ADR-8649): match it with a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ObjectiveKind {
    /// A team's flag: carried, dropped, returned and captured.
    Flag,
    /// The shared hill: a control volume that scores its sole occupant.
    Hill,
    /// A capture zone: a control volume with capture progress and an owner
    /// ([`crate::zones::Zone`]).
    Zone,
}

impl ObjectiveKind {
    /// The kind tag string ([`KIND_FLAG`], [`KIND_HILL`] or [`KIND_ZONE`]).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Flag => KIND_FLAG,
            Self::Hill => KIND_HILL,
            Self::Zone => KIND_ZONE,
        }
    }
}

/// An axis-aligned control volume: a horizontal footprint plus a height band.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HillVolume {
    /// Inclusive min corner (x, y, z).
    pub min: [f32; 3],
    /// Inclusive max corner (x, y, z).
    pub max: [f32; 3],
}

impl HillVolume {
    /// True when `pos` lies inside this volume (bounds inclusive).
    pub fn contains(&self, pos: [f32; 3]) -> bool {
        pos[0] >= self.min[0]
            && pos[0] <= self.max[0]
            && pos[1] >= self.min[1]
            && pos[1] <= self.max[1]
            && pos[2] >= self.min[2]
            && pos[2] <= self.max[2]
    }

    /// The volume's center, `(min + max) * 0.5` per axis.
    pub fn center(&self) -> [f32; 3] {
        [
            (self.min[0] + self.max[0]) * 0.5,
            (self.min[1] + self.max[1]) * 0.5,
            (self.min[2] + self.max[2]) * 0.5,
        ]
    }
}

/// One objective of the match (a flag, the hill or a capture zone). The host spawns an entity for it with the id
/// [`Objective::entity_id`] and mirrors [`Objective::position`] onto that entity after each step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Objective {
    /// The host's entity id for this objective (unique on the board).
    pub entity_id: u32,
    /// Flag, hill or zone.
    pub kind: ObjectiveKind,
    /// Owning fighting team for a flag (`1..=max_teams`); `0` for the shared hill and for a zone
    /// (a zone's owner changes and lives in [`Objective::zone`]).
    pub team_id: u8,
    /// The unit carrying a flag; always `None` for a hill.
    pub carrier_id: Option<u32>,
    /// A flag idle at its own base (the capture and return target).
    pub at_base: bool,
    /// Home position: a flag's base (it returns there), the hill's center.
    pub base_position: [f32; 3],
    /// Where the objective is now: the carrier's position while carried, the drop point while
    /// dropped, the base while home.
    pub position: [f32; 3],
    /// The control volume of a hill or a zone.
    pub volume: Option<HillVolume>,
    /// The match tick a flag dropped on (or, for a flag found lying with no tick, the first step
    /// that saw it): the start of its [`ObjectiveSettings::flag_return_ticks`] timer. `None` while
    /// the flag is home or carried, and always for a hill. Not serialized when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_tick: Option<u64>,
    /// A capture zone's settings and state (progress, owner); `None` for a flag or the hill. Not
    /// serialized when `None`, so flag and hill records keep their shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<crate::zones::Zone>,
}

impl Objective {
    /// True for a flag.
    pub fn is_flag(&self) -> bool {
        self.kind == ObjectiveKind::Flag
    }

    /// True for a hill.
    pub fn is_hill(&self) -> bool {
        self.kind == ObjectiveKind::Hill
    }

    /// True for a capture zone.
    pub fn is_zone(&self) -> bool {
        self.kind == ObjectiveKind::Zone
    }

    /// A dropped flag: not at base and not carried.
    pub fn is_dropped(&self) -> bool {
        self.is_flag() && !self.at_base && self.carrier_id.is_none()
    }
}

/// Sort `units` into the host order: entity id, then team.
///
/// Capture the flag gives a pickup to the first touching unit in the slice the host passes. Peers
/// that receive the same units in different orders and then call this agree on that winner.
pub fn host_unit_order(units: &mut [Contestant]) {
    units.sort_by(|a, b| a.id.cmp(&b.id).then(a.team.cmp(&b.team)));
}

/// One unit as the objective rules see it on a step. The host passes every unit that can touch an
/// objective, in its own deterministic entity order ([`host_unit_order`]): the first touching unit
/// in that order wins a pickup, so every peer must pass the same order.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Contestant {
    /// The unit's entity id.
    pub id: u32,
    /// Its team; `0` (neutral) and teams past the fighting teams never touch objectives.
    pub team: u8,
    /// Its position.
    pub position: [f32; 3],
    /// False for a dead unit or one at zero health: it never touches, holds or scores, and a flag it
    /// carried drops at its position.
    pub alive: bool,
}

/// Why a match (or, with rounds on, a round) ended.
///
/// Non-exhaustive since ADR-8485: match it with a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EndReason {
    /// A team reached the capture limit.
    CaptureLimitReached,
    /// A team reached the hill score limit.
    HillScoreLimitReached,
    /// The host or the game ended the match.
    Ended,
    /// The time limit ran out (the sole leader won, or a tie ended with no winner), or an
    /// overtime tie broke.
    TimeLimit,
    /// The match (or round) ended level: [`RulesEvent::MatchOver`] carries team 0.
    Draw,
    /// Every other fighting team forfeited (no seated, non-observer seat for the setting's
    /// ticks).
    Forfeit,
    /// A team won the rounds it needed, or led on round wins after the last round.
    RoundsWon,
    /// A team reached the zone score limit (domination, hardpoint; ADR-8649).
    ZoneScoreLimitReached,
    /// The attackers took the last zone of an attack/defend order (ADR-8649).
    ZonesCaptured,
    /// Every other side was eliminated: the last side standing won (`crate::elimination`).
    LastStanding,
    /// The other sides lost every unit of a `destroy_all_tagged` tag (`crate::victory`).
    Destroyed,
    /// A side held a `hold_for` objective for its ticks (`crate::victory`).
    Held,
    /// A side reached a `score_threshold` score (`crate::victory`).
    ScoreReached,
    /// The other sides surrendered (`crate::victory`).
    Surrendered,
}

impl EndReason {
    /// The reason as a lowercase string (`"capture_limit_reached"`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CaptureLimitReached => "capture_limit_reached",
            Self::HillScoreLimitReached => "hill_score_limit_reached",
            Self::Ended => "ended",
            Self::TimeLimit => "time_limit",
            Self::Draw => "draw",
            Self::Forfeit => "forfeit",
            Self::RoundsWon => "rounds_won",
            Self::ZoneScoreLimitReached => "zone_score_limit_reached",
            Self::ZonesCaptured => "zones_captured",
            Self::LastStanding => "last_standing",
            Self::Destroyed => "destroyed",
            Self::Held => "held",
            Self::ScoreReached => "score_reached",
            Self::Surrendered => "surrendered",
        }
    }
}

/// Why a dropped flag went home without a touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoReturnCause {
    /// It lay on the ground for [`ObjectiveSettings::flag_return_ticks`] match ticks.
    Timeout,
    /// The host's drop check answered [`crate::ctf::DropCheck::Return`] for
    /// where it lay (a kill volume, out of bounds, inside collision).
    DropCheck,
}

impl AutoReturnCause {
    /// The cause as a lowercase string (`"timeout"`, `"drop_check"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::DropCheck => "drop_check",
        }
    }
}

/// Why a seat's ledger row changed. Assists and deaths do not change the row's score.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoreReason {
    /// The seat scored a kill.
    Kill,
    /// The seat died.
    Death,
    /// The seat assisted a kill inside the assist window.
    Assist,
    /// The seat was credited one objective point (a capture or a hill point).
    Objective,
    /// The host or a logic graph added points (`MatchSession::add_seat_score`, the logic graph's
    /// `rules::AddScore`).
    Scripted,
}

/// Something the rules did on a step, for the HUD feed, the announcer, the match log and the agent.
/// Events never feed back into the rules.
///
/// New variants may be added in a minor release: match with a `_` arm.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RulesEvent {
    /// An enemy unit picked up a flag (from its base or the ground).
    FlagTaken {
        /// Match tick.
        tick: u64,
        /// The flag's entity id.
        flag: u32,
        /// The unit.
        unit: u32,
        /// The unit's team.
        team: u8,
        /// The flag's team.
        flag_team: u8,
    },
    /// A carried flag dropped: its carrier died or left.
    FlagDropped {
        /// Match tick.
        tick: u64,
        /// The flag's entity id.
        flag: u32,
        /// The carrier that dropped it.
        unit: u32,
        /// The flag's team.
        flag_team: u8,
        /// Where it lies.
        position: [f32; 3],
    },
    /// A unit returned its own dropped flag to base.
    FlagReturned {
        /// Match tick.
        tick: u64,
        /// The flag's entity id.
        flag: u32,
        /// The unit.
        unit: u32,
        /// The flag's team (the unit's team).
        flag_team: u8,
    },
    /// A carrier brought an enemy flag to its own base while its own flag was home.
    FlagCaptured {
        /// Match tick.
        tick: u64,
        /// The captured flag's entity id.
        flag: u32,
        /// The carrier.
        unit: u32,
        /// The scoring team.
        team: u8,
        /// The captured flag's team.
        flag_team: u8,
        /// The scoring team's score after the capture.
        score: u32,
    },
    /// The hill's sole occupant scored a point.
    HillPoint {
        /// Match tick.
        tick: u64,
        /// The scoring team.
        team: u8,
        /// Its score after the point.
        score: u32,
    },
    /// The match ended.
    MatchOver {
        /// Match tick.
        tick: u64,
        /// The winning team; `0` when it ended without a winner (for [`EndReason::Draw`], a draw).
        team: u8,
        /// Why.
        reason: EndReason,
    },
    /// The match was paused.
    Paused {
        /// Match tick (frozen while paused).
        tick: u64,
        /// The name on the pause banner.
        by: String,
    },
    /// The match resumed.
    Resumed {
        /// Match tick.
        tick: u64,
    },
    /// Every seat that must ready did; the next match starts at `at_step`.
    LaunchScheduled {
        /// The lockstep step the next match starts on.
        at_step: u64,
    },
    /// A scheduled launch was called off (a seat unreadied).
    LaunchCancelled {
        /// The lockstep step the cancel landed on.
        step: u64,
    },
    /// The next match started.
    Launched {
        /// The lockstep step it started on.
        step: u64,
        /// The playlist row, `None` when the playlist had no row left (the current settings rerun).
        index: Option<u32>,
        /// The row's map id (empty without a row).
        map_id: String,
        /// The row's mode string (empty without a row).
        mode: String,
    },
    /// A seat left: it is off the seated, ready and observer ledgers and off the join queue.
    SeatLeft {
        /// The lockstep step the leave landed on.
        step: u64,
        /// The team.
        team: u8,
        /// The slot.
        slot: u8,
    },
    /// The match entered a phase (only with a phase setting on, ADR-8485).
    PhaseChanged {
        /// The lockstep step it changed on.
        step: u64,
        /// The new phase.
        phase: MatchPhase,
        /// The round, from 1, with rounds on; `0` with rounds off.
        round: u32,
    },
    /// A round ended (rounds on). [`RulesEvent::MatchOver`] follows when it decided the match.
    RoundOver {
        /// Match tick.
        tick: u64,
        /// The round that ended, from 1.
        round: u32,
        /// The round's winner; `0` for none.
        team: u8,
        /// Why the round ended.
        reason: EndReason,
    },
    /// A dropped flag went home without a touch: its return timer ran out or the host's drop
    /// check returned it.
    FlagAutoReturned {
        /// Match tick.
        tick: u64,
        /// The flag's entity id.
        flag: u32,
        /// The flag's team.
        flag_team: u8,
        /// Why.
        cause: AutoReturnCause,
    },
    /// A seat's per-player ledger changed (a kill, a death, an assist or an objective point).
    ///
    /// Team scores on the board are a separate counter. This event does not change them.
    PlayerScored {
        /// Match tick.
        tick: u64,
        /// The seat's team.
        team: u8,
        /// The seat's slot.
        slot: u8,
        /// Why the row changed.
        reason: ScoreReason,
        /// The seat's score after the change.
        score: i64,
    },
    /// A dead seat comes back on this tick (the respawn policy released it, ADR-8647). The host
    /// spawns its pawn at the point [`crate::session::MatchSession::pick_spawn`] gives.
    RespawnAt {
        /// Match tick of the release.
        tick: u64,
        /// The seat's team.
        team: u8,
        /// The seat's slot.
        slot: u8,
    },
    /// A seat ran out of lives (or its policy never respawns) and is out of the match (ADR-8647).
    Eliminated {
        /// Match tick of the death that ended it.
        tick: u64,
        /// The seat's team.
        team: u8,
        /// The seat's slot.
        slot: u8,
    },
    /// A capture zone changed owner: its progress filled for `team` (ADR-8649).
    ZoneCaptured {
        /// Match tick.
        tick: u64,
        /// The zone's entity id.
        zone: u32,
        /// The new owner.
        team: u8,
        /// The owner before; `0` when the zone was neutral.
        previous: u8,
    },
    /// A zone's owner scored a point (every `owner_scores_every` ticks it holds the zone).
    ZonePoint {
        /// Match tick.
        tick: u64,
        /// The zone's entity id.
        zone: u32,
        /// The owner that scored.
        team: u8,
        /// Its team score after the point.
        score: u32,
    },
    /// A zone unlocked: the next hardpoint of the rotation, or the next zone of an attack/defend
    /// order.
    ZoneActivated {
        /// Match tick.
        tick: u64,
        /// The zone's entity id.
        zone: u32,
    },
    /// A co-op wave started ([`crate::waves`]).
    WaveStarted {
        /// Match tick.
        tick: u64,
        /// Index into the wave table.
        wave: u32,
    },
    /// The wave director released `count` enemies of one group: the game spawns that many of
    /// `archetype` at points tagged `spawn_tag` and tags each with `wave`.
    WaveSpawn {
        /// Match tick.
        tick: u64,
        /// Index into the wave table.
        wave: u32,
        /// Index of the group in the wave.
        group: u32,
        /// The game's archetype id.
        archetype: String,
        /// Enemies to spawn now.
        count: u32,
        /// The spawn tag.
        spawn_tag: String,
    },
    /// Every enemy of a wave was released and reported down.
    WaveCleared {
        /// Match tick.
        tick: u64,
        /// Index into the wave table.
        wave: u32,
    },
    /// The co-op match ended: won on every wave cleared or the objective reached, lost on a team
    /// wipe or the objective destroyed. A `match_over` follows.
    CoopEnded {
        /// Match tick.
        tick: u64,
        /// True on a win.
        won: bool,
        /// Why.
        cause: crate::waves::CoopCause,
    },
    /// A racer passed a checkpoint that was not the start ([`crate::race`]).
    RacerCheckpoint {
        /// Match tick.
        tick: u64,
        /// The racer's team.
        team: u8,
        /// The racer's slot.
        slot: u8,
        /// The checkpoint id.
        checkpoint: u32,
        /// Laps completed so far.
        lap: u32,
    },
    /// A racer completed a lap.
    RacerLap {
        /// Match tick.
        tick: u64,
        /// The racer's team.
        team: u8,
        /// The racer's slot.
        slot: u8,
        /// Laps completed, this one included.
        lap: u32,
        /// The lap's length in match ticks.
        lap_ticks: u64,
        /// True when it is the racer's best lap so far.
        best: bool,
    },
    /// A racer entered a checkpoint that does not follow its last one: nothing counts.
    RacerMissedCheckpoint {
        /// Match tick.
        tick: u64,
        /// The racer's team.
        team: u8,
        /// The racer's slot.
        slot: u8,
        /// The checkpoint it entered.
        checkpoint: u32,
        /// The first checkpoint it should pass next.
        expected: u32,
    },
    /// A racer lost ground on its segment for `wrong_way_ticks` steps in a row.
    RacerWrongWay {
        /// Match tick.
        tick: u64,
        /// The racer's team.
        team: u8,
        /// The racer's slot.
        slot: u8,
    },
    /// A racer finished.
    RacerFinished {
        /// Match tick.
        tick: u64,
        /// The racer's team.
        team: u8,
        /// The racer's slot.
        slot: u8,
        /// Finish place, from 1.
        place: u32,
    },
    /// A racer did not finish inside the window after the first finisher.
    RacerDnf {
        /// Match tick.
        tick: u64,
        /// The racer's team.
        team: u8,
        /// The racer's slot.
        slot: u8,
    },
    /// The shrinking zone entered a stage (`crate::zone`). Positions are XZ, world units.
    ZonePhase {
        /// Match tick.
        tick: u64,
        /// The phase, from 0 (the phase count once closed).
        phase: u32,
        /// The new stage.
        stage: crate::zone::ZoneStage,
        /// Center of the current circle.
        center: [f32; 2],
        /// Radius of the current circle.
        radius: f32,
        /// Center of the circle this phase shrinks to.
        target_center: [f32; 2],
        /// Radius of the circle this phase shrinks to.
        target_radius: f32,
    },
    /// A live unit is outside the zone on this tick. The game applies the damage (the rules core
    /// never owns health).
    ZoneDamage {
        /// Match tick.
        tick: u64,
        /// The unit's entity id.
        unit: u32,
        /// Damage for this tick.
        amount: u32,
    },
    /// A team is out of the match (`crate::elimination`).
    TeamEliminated {
        /// Match tick.
        tick: u64,
        /// The team.
        team: u8,
        /// Its place (1 is first).
        place: u8,
        /// Why it went out.
        cause: crate::elimination::OutCause,
    },
    /// A side won: every contender of the winning alliance (or the one team), sorted. Sent before
    /// the [`RulesEvent::MatchOver`] it decides; empty for a draw.
    Victory {
        /// Match tick.
        tick: u64,
        /// The winning teams.
        teams: Vec<u8>,
        /// Why.
        reason: EndReason,
    },
    /// A seat voted on its team's surrender (`crate::victory`).
    SurrenderVote {
        /// Match tick.
        tick: u64,
        /// The team.
        team: u8,
        /// The slot.
        slot: u8,
        /// Yes or no.
        yes: bool,
    },
    /// A surrender ballot failed: its window ran out or the no votes made it unreachable.
    SurrenderFailed {
        /// Match tick.
        tick: u64,
        /// The team.
        team: u8,
        /// The first match tick the team may vote again.
        until: u64,
    },
    /// The rotation passed over a playlist row on a launch (ADR-8897). Sent before the
    /// [`RulesEvent::Launched`] it explains; when no row fits, the current row reruns.
    RowSkipped {
        /// The lockstep step of the launch.
        step: u64,
        /// The row's playlist index.
        index: u32,
        /// The row's map id.
        map_id: String,
        /// Why: `too_few_players`, `too_many_players`, `missing_content` or `recent`.
        reason: crate::playlist::SkipReason,
    },
}

/// Objective limits and settings of a match (the caps a game used to hard-code).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ObjectiveSettings {
    /// Fighting teams are `1..=max_teams`; team 0 is neutral.
    pub max_teams: u8,
    /// Objectives the board accepts.
    pub max_objectives: u16,
    /// Flag touch radius across XZ, world units.
    pub flag_touch_radius: f32,
    /// Vertical touch band, world units. `None` (the default, S-14's rule) touches across XZ only,
    /// so on stacked floors a unit directly under a flag touches it through the ceiling. `Some(h)`
    /// also needs the height difference `dy` to satisfy `dy * dy <= h * h` (a band of `|h|` above
    /// and below). It applies to pickups, returns and captures (a carrier at its own base). Hill
    /// volumes are already 3-D boxes and ignore it.
    pub touch_height: Option<f32>,
    /// A dropped flag nobody touches goes home at the end of the step on match tick
    /// `drop tick + flag_return_ticks` (a `flag_auto_returned` event, cause `timeout`). A pickup
    /// clears the timer; the next drop starts a new one. `None` (the default, S-14's rule): a
    /// dropped flag waits for a teammate's touch.
    pub flag_return_ticks: Option<u64>,
    /// A capture needs the scoring team's own flag home (idle at its base). `true` is the default
    /// and S-14's rule; `false` scores a capture while the own flag is away.
    pub capture_needs_home_flag: bool,
    /// Captures to win capture the flag; `0` reads as `1`.
    pub capture_limit: u32,
    /// Hill score to win king of the hill; `0` reads as `1`.
    pub hill_score_limit: u32,
    /// Match ticks per hill accrual (one point per period); `0` reads as `1`. The default is one
    /// second at the default tick rate.
    pub hill_accrual_ticks: u32,
    /// Pending events kept on the board and, when a session is created, on the flow. The default
    /// is [`DEFAULT_MAX_PENDING_EVENTS`]. `0` means that default. Oldest events past the cap are
    /// dropped and counted on [`ObjectiveBoard::events_dropped`].
    #[serde(default = "default_max_pending_events")]
    pub max_pending_events: u32,
}

impl Default for ObjectiveSettings {
    fn default() -> Self {
        Self {
            max_teams: DEFAULT_MAX_TEAMS,
            max_objectives: DEFAULT_MAX_OBJECTIVES,
            flag_touch_radius: DEFAULT_FLAG_TOUCH_RADIUS,
            touch_height: None,
            flag_return_ticks: None,
            capture_needs_home_flag: true,
            capture_limit: DEFAULT_CAPTURE_LIMIT,
            hill_score_limit: DEFAULT_HILL_SCORE_LIMIT,
            hill_accrual_ticks: regia_detsim::tick::TICK_RATE_HZ,
            max_pending_events: DEFAULT_MAX_PENDING_EVENTS,
        }
    }
}

impl ObjectiveSettings {
    /// The capture limit in force (never below 1).
    pub fn effective_capture_limit(&self) -> u32 {
        self.capture_limit.max(1)
    }

    /// The hill score limit in force (never below 1).
    pub fn effective_hill_score_limit(&self) -> u32 {
        self.hill_score_limit.max(1)
    }

    /// The hill accrual period in force (never below 1).
    pub fn effective_hill_accrual_ticks(&self) -> u32 {
        self.hill_accrual_ticks.max(1)
    }

    /// Pending events kept before the oldest is dropped. Never zero: a stored `0` is the default.
    pub fn effective_max_pending_events(&self) -> usize {
        pending_event_cap(self.max_pending_events)
    }

    /// True for a fighting team (`1..=max_teams`).
    pub fn is_fighting_team(&self, team: u8) -> bool {
        (1..=self.max_teams).contains(&team)
    }

    /// True when a unit at `unit` touches a flag or base at `target`: inside
    /// [`ObjectiveSettings::flag_touch_radius`] across XZ and, when
    /// [`ObjectiveSettings::touch_height`] is set, inside its height band. `+ - *` and comparisons
    /// only; with `touch_height: None` it computes S-14's bits.
    pub fn flag_touches(&self, unit: [f32; 3], target: [f32; 3]) -> bool {
        let r = self.flag_touch_radius;
        // `<=` as S-14 wrote it: a NaN distance never touches.
        let across = distance_sq_xz(unit, target) <= r * r;
        match self.touch_height {
            None => across,
            Some(h) => {
                let dy = unit[1] - target[1];
                across && dy * dy <= h * h
            }
        }
    }
}

/// The match's objectives, team scores, match-over state and the events not yet drained.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectiveBoard {
    /// The mode the board scores.
    pub mode: MatchMode,
    /// Limits and settings.
    pub settings: ObjectiveSettings,
    /// Flags and the hill, in placement order.
    pub objectives: Vec<Objective>,
    /// Score per team, indexed by team id (`0` is neutral and never scores).
    pub team_scores: Vec<u32>,
    /// True once a limit is reached (or the host ended the match): nothing scores after it.
    pub match_over: bool,
    /// The match tick the match ended on.
    pub match_over_tick: Option<u64>,
    /// Why it ended.
    pub end_reason: Option<EndReason>,
    /// Events since the last [`ObjectiveBoard::drain_events`].
    #[serde(skip)]
    pub events: Vec<RulesEvent>,
    /// Events dropped because the pending cap was full. Oldest first, and never silent:
    /// `rules.match_get` serializes this counter. A host that drains every step leaves it at 0.
    #[serde(default)]
    pub events_dropped: u64,
    /// How the capture zones play together (domination, hardpoint, attack/defend; ADR-8649). Not
    /// serialized while it is the default, so a board without zone rules keeps its shape.
    #[serde(default, skip_serializing_if = "crate::zones::ZoneRules::is_default")]
    pub zone_rules: crate::zones::ZoneRules,
    /// Objective ids a host or a logic graph locked ([`ObjectiveBoard::set_locked`]), sorted and
    /// without duplicates. A locked flag cannot be taken, returned by touch or captured; a locked
    /// hill or capture zone scores no points. Empty unless something locks one, and not serialized while empty, so
    /// a board that never locks keeps its bits.
    /// A file or snapshot that stores the list out of order or with repeats reads back sorted and
    /// unique, and [`ObjectiveBoard::set_locked`] and [`ObjectiveBoard::is_locked`] do not rely on
    /// the order of a list a host wrote into the field directly.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_locked"
    )]
    pub locked: Vec<u32>,
}

/// Reads `ObjectiveBoard::locked` sorted and without duplicates, whatever order the file kept.
fn deserialize_locked<'de, D>(deserializer: D) -> Result<Vec<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut ids = Vec::<u32>::deserialize(deserializer)?;
    canonical_locked(&mut ids);
    Ok(ids)
}

/// Sorts and dedups a lock list in place (the one order every peer and the proof hash use).
pub(crate) fn canonical_locked(ids: &mut Vec<u32>) {
    if !ids.is_sorted_by(|a, b| a < b) {
        ids.sort_unstable();
        ids.dedup();
    }
}

impl ObjectiveBoard {
    /// An empty board for `mode`.
    pub fn new(mode: MatchMode, settings: ObjectiveSettings) -> Self {
        Self {
            mode,
            settings,
            objectives: Vec::new(),
            team_scores: vec![0; usize::from(settings.max_teams) + 1],
            match_over: false,
            match_over_tick: None,
            end_reason: None,
            events: Vec::new(),
            events_dropped: 0,
            zone_rules: crate::zones::ZoneRules::default(),
            locked: Vec::new(),
        }
    }

    /// Lock (`on`) or unlock objective `id`. Returns false, changing nothing, when the board has no
    /// objective `id`. A locked flag takes no touches: nobody picks it up or returns it by touch,
    /// and a carrier holding it does not capture (a flag carried when it is locked stays carried,
    /// and a dead carrier still drops it; drop checks and the return timer still send it home). A
    /// locked hill scores no points. A locked capture zone takes no progress and scores nothing, on
    /// top of its zone mode's own lock (ADR-8731). Locks survive a round start (`reset_play`) and
    /// are cleared by a new match (`reset`).
    pub fn set_locked(&mut self, id: u32, on: bool) -> bool {
        if self.objective(id).is_none() {
            return false;
        }
        // The field is public: a host or an old snapshot may have left it unsorted or repeated.
        canonical_locked(&mut self.locked);
        match (self.locked.binary_search(&id), on) {
            (Err(at), true) => self.locked.insert(at, id),
            (Ok(at), false) => {
                self.locked.remove(at);
            }
            _ => {}
        }
        true
    }

    /// True when objective `id` is locked ([`ObjectiveBoard::set_locked`]).
    pub fn is_locked(&self, id: u32) -> bool {
        // A linear scan: correct for a list in any order (a binary search of an unsorted list is
        // unspecified), and the list holds a handful of ids.
        self.locked.contains(&id)
    }

    /// Clear objectives, scores and the match-over state for a new match of `mode` (settings kept).
    /// Pending events and [`ObjectiveBoard::events_dropped`] stay, so a host that has not drained
    /// still sees them after a launch.
    pub fn reset(&mut self, mode: MatchMode) {
        let events = std::mem::take(&mut self.events);
        let events_dropped = self.events_dropped;
        *self = Self::new(mode, self.settings);
        self.events = events;
        self.events_dropped = events_dropped;
    }

    /// Push one event, dropping the oldest when [`ObjectiveSettings::max_pending_events`] is full.
    pub(crate) fn push_event(&mut self, event: RulesEvent) {
        push_capped(
            &mut self.events,
            &mut self.events_dropped,
            self.settings.max_pending_events,
            event,
        );
    }

    /// Objectives on the board (each counts against the cap).
    pub fn live_objective_count(&self) -> usize {
        self.objectives.len()
    }

    /// `team`'s score (0 for a team past the board).
    pub fn score(&self, team: u8) -> u32 {
        self.team_scores.get(usize::from(team)).copied().unwrap_or(0)
    }

    /// The objective with entity id `id`.
    pub fn objective(&self, id: u32) -> Option<&Objective> {
        self.objectives.iter().find(|o| o.entity_id == id)
    }

    /// Take the events since the last drain, oldest first.
    pub fn drain_events(&mut self) -> Vec<RulesEvent> {
        std::mem::take(&mut self.events)
    }

    /// End the match now (host or game decision) at match tick `tick`, with `team` as the winner
    /// (`0` for none). Does nothing when it is already over.
    pub fn end_match(&mut self, tick: u64, team: u8, reason: EndReason) {
        if self.match_over {
            return;
        }
        self.match_over = true;
        self.match_over_tick = Some(tick);
        self.end_reason = Some(reason);
        self.push_event(RulesEvent::MatchOver { tick, team, reason });
    }

    /// Clear the scores and the match-over state and send every flag home (its carrier dropped),
    /// keeping the objectives: the start of a round, or the end of a warm-up.
    pub fn reset_play(&mut self) {
        self.team_scores.fill(0);
        self.match_over = false;
        self.match_over_tick = None;
        self.end_reason = None;
        for flag in self.objectives.iter_mut().filter(|o| o.is_flag()) {
            flag.carrier_id = None;
            flag.at_base = true;
            flag.position = flag.base_position;
        }
        self.reset_zones();
    }

    /// `(team, base)` of every flag, in placement order.
    pub fn flag_bases(&self) -> Vec<(u8, [f32; 3])> {
        self.objectives
            .iter()
            .filter(|o| o.is_flag())
            .map(|o| (o.team_id, o.base_position))
            .collect()
    }

    /// Undo the match end the objective rules recorded on this step from `events_from` on (a
    /// round end with rounds on): the board plays on and the `MatchOver` event is dropped.
    pub(super) fn reopen(&mut self, events_from: usize) {
        self.match_over = false;
        self.match_over_tick = None;
        self.end_reason = None;
        let start = events_from.min(self.events.len());
        let mut index = start;
        while index < self.events.len() {
            if matches!(self.events[index], RulesEvent::MatchOver { .. }) {
                self.events.remove(index);
            } else {
                index += 1;
            }
        }
    }

    /// The cap and id checks every objective placement shares.
    pub(super) fn check_room(&self, entity_id: u32) -> Result<(), super::RulesError> {
        if self.live_objective_count() >= usize::from(self.settings.max_objectives) {
            return Err(super::RulesError::ObjectiveCap {
                cap: self.settings.max_objectives,
            });
        }
        if self.objective(entity_id).is_some() {
            return Err(super::RulesError::ObjectiveId { id: entity_id });
        }
        Ok(())
    }

    /// Add one point to `team` and return its new score.
    pub(super) fn add_point(&mut self, team: u8) -> u32 {
        let Some(score) = self.team_scores.get_mut(usize::from(team)) else {
            return 0;
        };
        *score = score.saturating_add(1);
        *score
    }
}

/// Squared distance across XZ (`dx * dx + dz * dz`, in that order).
pub fn distance_sq_xz(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dz = a[2] - b[2];
    dx * dx + dz * dz
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hill_volume_bounds_are_inclusive_and_center_is_the_midpoint() {
        let v = HillVolume {
            min: [-10.0, -1.0, -10.0],
            max: [10.0, 20.0, 10.0],
        };
        assert!(v.contains([10.0, 20.0, -10.0]));
        assert!(!v.contains([10.01, 0.0, 0.0]));
        assert!(!v.contains([0.0, 20.5, 0.0]));
        assert_eq!(v.center(), [0.0, 9.5, 0.0]);
    }

    #[test]
    fn kind_tags_are_not_entity_names() {
        assert_eq!(ObjectiveKind::Flag.as_str(), KIND_FLAG);
        assert_eq!(ObjectiveKind::Hill.as_str(), KIND_HILL);
        assert_eq!(ObjectiveKind::Zone.as_str(), KIND_ZONE);
        assert_ne!(OBJECTIVE_ENTITY_NAME, KIND_FLAG);
        assert_ne!(OBJECTIVE_ENTITY_NAME, KIND_HILL);
        assert_ne!(OBJECTIVE_ENTITY_NAME, KIND_ZONE);
    }

    #[test]
    fn limits_never_drop_to_zero() {
        let s = ObjectiveSettings {
            capture_limit: 0,
            hill_score_limit: 0,
            hill_accrual_ticks: 0,
            ..ObjectiveSettings::default()
        };
        assert_eq!(s.effective_capture_limit(), 1);
        assert_eq!(s.effective_hill_score_limit(), 1);
        assert_eq!(s.effective_hill_accrual_ticks(), 1);
        assert_eq!(ObjectiveSettings::default().hill_accrual_ticks, 60);
    }

    #[test]
    fn a_board_sizes_its_scores_from_the_team_setting() {
        let six = ObjectiveSettings {
            max_teams: 6,
            ..ObjectiveSettings::default()
        };
        let mut board = ObjectiveBoard::new(MatchMode::CaptureTheFlag, six);
        assert_eq!(board.team_scores.len(), 7);
        assert_eq!(board.add_point(6), 1);
        assert_eq!(board.add_point(7), 0, "a team past the board never scores");
        board.end_match(9, 6, EndReason::Ended);
        board.end_match(10, 1, EndReason::Ended);
        assert_eq!(board.match_over_tick, Some(9), "the first end stands");
        assert_eq!(board.drain_events().len(), 1);
        board.reset(MatchMode::KingOfTheHill);
        assert_eq!(board.team_scores, vec![0; 7]);
        assert!(!board.match_over);
        assert_eq!(board.mode, MatchMode::KingOfTheHill);
    }

    #[test]
    fn a_missing_event_cap_decodes_as_the_default_and_the_counter_is_visible() {
        let parsed: ObjectiveSettings = serde_json::from_str("{}").expect("settings");
        assert_eq!(parsed.max_pending_events, DEFAULT_MAX_PENDING_EVENTS);
        let zero: ObjectiveSettings = serde_json::from_str(r#"{"max_pending_events":0}"#).expect("zero cap");
        assert_eq!(zero.effective_max_pending_events(), 4096);
        let board = ObjectiveBoard::new(MatchMode::TeamDeathmatch, ObjectiveSettings::default());
        let value = serde_json::to_value(&board).expect("board");
        assert_eq!(value["events_dropped"], 0);
        assert!(
            value.get("events").is_none(),
            "the event log stays out of match_get"
        );
    }
}
