// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/match_flow.rs, commit d6ed37677), ADR-8012.
// Mark: adapt (lines 285-295 and 323-331: launching the next playlist row and applying its mode).
// Named issues: the launch runs on the flow's agreed step instead of inside the ready input, the board
// is reset for the row's mode instead of the game's mode flags and presets, and the session steps the
// flow, the playlist and the objective rules together in one fixed order.

//! One hosted match session: the flow, the playlist and the objective board, stepped together once
//! per lockstep step.

use serde::{Deserialize, Serialize};

use super::RulesError;
use super::ctf::DropCheck;
use super::elimination::{EliminationBook, EliminationRules, OutCause, Placement, teams_without_seats};
use super::end_stats::MatchEndLedger;
use super::map_vote::{MapVote, ServerMaps, VoteReject};
use super::match_flow::{FlowInput, FlowSettings, FlowStep, Issuer, MatchClock, MatchFlow, SeatId};
use super::mode::MatchMode;
use super::objective::{Contestant, EndReason, HillVolume, ObjectiveBoard, ObjectiveSettings, RulesEvent};
use super::phases::PhaseSettings;
use super::playlist::{Playlist, PlaylistEntry};
use super::race::{BEST_LAP_COUNTER, Race, RaceSettings, Standing};
use super::series::{RoundResult, SERIES_TEAM_SLOTS, SeriesState};
use super::spawn::RespawnPolicy;
use super::spawn::{
    RespawnBook, RespawnRules, SpawnAsk, SpawnChoice, SpawnPointData, SpawnState, SpawnTerms, SpawnWeights,
};
use super::teams::{LedgerBook, SeatMove, TeamRules};
use super::victory::{VictoryCondition, VictoryRules, VictoryState};
use super::waves::{CoopObjective, CoopView, CoopWaves, WaveSettings};
use super::win_condition::{MatchView, Outcome, WinCondition, WinRuleState};
use super::zone::{ZoneSchedule, ZoneState};

/// Settings of a session: every peer of a lockstep match runs the same ones.
///
/// The team list keeps this from being `Copy`. A struct update of [`SessionSettings::default`]
/// still moves the temporary.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionSettings {
    /// Pause, ready-up and seat settings.
    pub flow: FlowSettings,
    /// Objective, score-limit and team settings.
    pub objectives: ObjectiveSettings,
    /// Team definitions, switch policy and the assist window. Default: four unnamed teams, no
    /// switching, no rebalance.
    pub teams: TeamRules,
    /// Respawn policy per team and the spawn-point policy (ADR-8647). Default: instant respawn,
    /// the first eligible point; nothing is booked until the host reports a death.
    pub respawn: RespawnRules,
    /// Co-op waves ([`crate::waves`]). Default: an empty table, off.
    pub waves: WaveSettings,
    /// The race course ([`crate::race`]). Default: no checkpoints, off.
    pub race: RaceSettings,
    /// A shrinking zone (battle royale, GR-11.3). `None` (the default, S-14's behavior): no zone.
    pub zone: Option<ZoneSchedule>,
    /// Team elimination, placements and alliances. Default: off.
    pub elimination: EliminationRules,
    /// Strategy victory conditions and the surrender vote (GR-11.4). Default: none.
    pub victory: VictoryRules,
}

/// The shape version of a serialized [`MatchSession`] (ADR-8881). A session this build writes
/// carries it as its `version` key; a session written before the key existed (v0.6) reads as
/// version 1. Bump it with every shape change of the session or of a type it holds.
pub const MATCH_SESSION_VERSION: u32 = 1;

fn match_session_version_default() -> u32 {
    MATCH_SESSION_VERSION
}

/// Reads `version`: refuses 0 and a version newer than [`MATCH_SESSION_VERSION`] by name
/// (`match_session_version`), so a newer build's session is not half read.
fn read_match_session_version<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let version = u32::deserialize(deserializer)?;
    if version == 0 || version > MATCH_SESSION_VERSION {
        return Err(serde::de::Error::custom(format!(
            "match_session_version: version {version} is not 1..={MATCH_SESSION_VERSION} (this build reads up to \
             {MATCH_SESSION_VERSION})"
        )));
    }
    Ok(version)
}

/// The playlist row a session is playing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentRow {
    /// Its playlist index.
    pub index: u32,
    /// The row.
    pub entry: PlaylistEntry,
}

/// What one [`MatchSession::step`] did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStep {
    /// The flow's step.
    pub flow: FlowStep,
    /// Events of this step (flow first, then objectives), oldest first.
    pub events: Vec<RulesEvent>,
}

/// A hosted match session.
///
/// Per lockstep step the host applies that step's inputs with [`MatchSession::apply`] (in the
/// agreed input order), then calls [`MatchSession::step`] with its units in its deterministic
/// entity order. A peer applies one seat's inputs with [`MatchSession::apply_from`]; the issuer
/// comes from the session layer, not from the input. When the flow starts the next match, the next playlist row is taken, the board is
/// reset for the row's mode, and the game loads the row's map and places its objectives
/// ([`ObjectiveBoard::spawn_flag`], [`ObjectiveBoard::install_hill_volume`]).
///
/// ```
/// use regia_rules::match_flow::{FlowInput, FlowSettings};
/// use regia_rules::playlist::PlaylistEntry;
/// use regia_rules::session::{MatchSession, SessionSettings};
///
/// let mut session = MatchSession::new(SessionSettings {
///     flow: FlowSettings { start_delay_ticks: 2, ..FlowSettings::default() },
///     ..SessionSettings::default()
/// });
/// session.playlist.set(vec![PlaylistEntry::new("harbor", "ctf")]);
/// session.apply(&FlowInput::Seat { team: 1, slot: 0 }).unwrap();
/// session.apply(&FlowInput::OpenBetween).unwrap();
/// session.apply(&FlowInput::Ready { team: 1, slot: 0 }).unwrap();
/// let steps: Vec<bool> = (0..3).map(|_| session.step(&[]).flow.launched).collect();
/// assert_eq!(steps, [false, false, true]);
/// assert_eq!(session.current.as_ref().map(|row| row.entry.map_id.as_str()), Some("harbor"));
/// ```
///
/// Serialized (a snapshot, a hot reload, the agent's `rules.match_*` results), a session carries
/// its shape version ([`MATCH_SESSION_VERSION`]) and refuses keys this build does not know
/// (ADR-8881).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchSession {
    /// The shape version ([`MATCH_SESSION_VERSION`]). Absent in a v0.6 session, which reads as 1;
    /// 0 or a newer version is refused (`match_session_version`).
    #[serde(
        default = "match_session_version_default",
        deserialize_with = "read_match_session_version"
    )]
    pub version: u32,
    /// Pause, ready-up and observers.
    pub flow: MatchFlow,
    /// The rows to play.
    pub playlist: Playlist,
    /// Objectives, scores and the match-over state of the running match.
    pub board: ObjectiveBoard,
    /// The row now playing (`None` before the first start).
    pub current: Option<CurrentRow>,
    /// Best-of series. A single match until the host sets a length.
    #[serde(default)]
    pub series: SeriesState,
    /// The open end-of-match vote, when the host enabled one.
    #[serde(default)]
    pub vote: Option<MapVote>,
    /// End-of-match ledger. Objective events do not carry kills; the game calls [`MatchSession::note_kill`]
    /// and [`MatchSession::note_structure`]. A launch clears the ledger. A match-over stamps it.
    #[serde(default)]
    pub ledger: MatchEndLedger,
    /// Rotation and the vote flag. The vote stays off until this flag is set.
    #[serde(default)]
    pub maps: ServerMaps,
    /// Opaque carry the game sets before a round is booked. Index is the team.
    #[serde(default)]
    pub carry: [u32; SERIES_TEAM_SLOTS],
    /// Players the rotation and [`Playlist::vote_candidates`] see: a launch skips rows outside
    /// their player gate (ADR-8897). 0 keeps rows that set no minimum. The host sets it from its
    /// seated players before a launch.
    #[serde(default)]
    pub player_count: u32,
    /// The win rule. Lives until the host picks another.
    #[serde(default)]
    pub win_rule: WinRuleState,
    /// Team definitions and switch policy for this session. Default: S-14's four unnamed teams.
    #[serde(default)]
    pub teams: TeamRules,
    /// Per-seat kills, deaths, assists, objective points and named counters.
    #[serde(default)]
    pub ledgers: LedgerBook,
    /// Next party id handed out by [`MatchSession::assign_team`]. `0` means none yet; a party id
    /// is never `0` (that marks a player seated alone).
    #[serde(default)]
    pub next_party: u64,
    /// Seat moves since the last launch, oldest first: team changes and slot moves from
    /// [`MatchSession::assign_team`], then the launch rebalance. A host that tags entities with a
    /// team and a slot reads (or drains) these and retags them in the same step, so a hit after a
    /// move credits the moved row. A launch clears the list before its rebalance (ADR-8657).
    #[serde(default)]
    pub seat_moves: Vec<SeatMove>,
    /// Respawn and spawn-point settings (ADR-8647).
    #[serde(default)]
    pub respawn: RespawnRules,
    /// Pending respawns, lives and eliminated seats of the running match.
    #[serde(default)]
    pub respawns: RespawnBook,
    /// The spawn picker's generator, round-robin cursor and recent uses.
    #[serde(default)]
    pub spawn: SpawnState,
    /// The skeleton mode of each rule-set id the playlist's rows name (ADR-8694), filled by the
    /// host from `ruleset::RuleSetLibrary::rule_modes` before the match. A row whose `mode` is a
    /// key here launches on that mode; any other row parses its mode string as before (S-14).
    /// Every peer sets the same map: the rule sets' content hash is part of the handshake.
    /// Not serialized while empty, so a session without rule sets keeps its shape.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub rule_modes: std::collections::BTreeMap<String, MatchMode>,
    /// Co-op wave settings and the director's state. Off (never stepped) with an empty table.
    #[serde(default)]
    pub waves: CoopWaves,
    /// Race settings and state. Off (never stepped) with an empty course.
    #[serde(default)]
    pub race: Race,
    /// The zone schedule (`SessionSettings::zone`).
    #[serde(default)]
    pub zone: Option<ZoneSchedule>,
    /// The running zone of the match.
    #[serde(default)]
    pub zone_state: ZoneState,
    /// Elimination settings (`SessionSettings::elimination`).
    #[serde(default)]
    pub elimination: EliminationRules,
    /// Contenders, the elimination order and the winners of the running match.
    #[serde(default)]
    pub placements: EliminationBook,
    /// Victory conditions (`SessionSettings::victory`).
    #[serde(default)]
    pub victory: VictoryRules,
    /// Host reports and surrender ballots of the running match.
    #[serde(default)]
    pub victory_state: VictoryState,
    /// Map and rule-set ids missing from the installed content, filled by the host (ADR-8897). A
    /// row whose `map_id` or `mode` is listed is skipped on a launch and left off the map vote
    /// (`missing_content`). Every peer sets the same list (the content hash is in the handshake).
    /// Not serialized while empty.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub missing_content: std::collections::BTreeSet<String>,
    /// The team the latest [`RulesEvent::MatchOver`] named (`0` names no team), read while the
    /// board is over: a forfeit or a condition winner may not lead on points. Kept in the session,
    /// not only in the one-shot event, so it survives a snapshot or a hot reload
    /// (rules-match-state-replication-review finding A). A server-authoritative replication layer
    /// also stores the winner it saw here. Not serialized while `None`; not in the proof hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winner: Option<u8>,
    /// The lockstep step the running match launched on (`0` before the first launch). Not
    /// serialized while `0`; not in the proof hash (ADR-8652).
    #[serde(default, skip_serializing_if = "is_zero_step")]
    pub launched_step: u64,
    /// How the running match ended, once it has (`None` while it plays). Set where the session's
    /// events are drained, so a host end between steps is seen too; a launch clears it. Not
    /// serialized while `None`; not in the proof hash (ADR-8652).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_end: Option<MatchEnd>,
    /// Coordinates a player left or moved off and nobody holds since, sorted (ADR-8881). A credit
    /// to one of them ([`MatchSession::credit_kill`], [`MatchSession::note_hit`]) books nothing
    /// and opens no ledger row: a departed player's pawn that still kills or dies does not put
    /// that player back on the score sheet. A coordinate nobody ever held (a game's unseated
    /// bots) is credited as before. Seating the coordinate again takes it off the list. Not
    /// serialized while empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vacated: Vec<SeatId>,
}

/// `serde` skip test for [`MatchSession::launched_step`].
fn is_zero_step(step: &u64) -> bool {
    *step == 0
}

/// The slots of `team` in `seats` (sorted) that are not `eliminated`: the seats a surrender
/// ballot counts.
fn voting_slots(seats: &[SeatId], eliminated: &[SeatId], team: u8) -> Vec<u8> {
    seats
        .iter()
        .filter(|s| s.team == team && eliminated.binary_search(s).is_err())
        .map(|s| s.slot)
        .collect()
}

/// How a match ended: the step and tick, the winner and the reason ([`MatchSession::match_end`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchEnd {
    /// The lockstep step the end was drained on.
    pub step: u64,
    /// The match tick it ended on.
    pub tick: u64,
    /// The winning team; `0` for none (a draw or no winner).
    pub winner: u8,
    /// Why it ended.
    pub reason: EndReason,
}

/// Voter id for `seat` on a ballot. A guest may cast only this id.
pub fn voter_id(seat: SeatId) -> u32 {
    u32::from(seat.team) << 8 | u32::from(seat.slot)
}

impl MatchSession {
    /// A running session with an empty playlist and a board for team deathmatch.
    ///
    /// The running match opens in its first phase (the warm-up, the countdown or live play) when
    /// a phase setting is on. No `phase_changed` event is sent for it: a host starts its first
    /// match with [`MatchSession::start_now`] or the ready gate, which sends one.
    pub fn new(settings: SessionSettings) -> Self {
        let spawn = SpawnState::seeded(settings.respawn.seed);
        let mut flow = MatchFlow::new(settings.flow);
        flow.max_pending_events = settings.objectives.max_pending_events;
        let phases = settings.flow.phases;
        flow.phases.start_match(&phases, 0, &mut Vec::new());
        Self {
            version: MATCH_SESSION_VERSION,
            flow,
            playlist: Playlist::default(),
            board: ObjectiveBoard::new(MatchMode::TeamDeathmatch, settings.objectives),
            current: None,
            series: SeriesState::default(),
            vote: None,
            ledger: MatchEndLedger::default(),
            maps: ServerMaps::default(),
            carry: [0; SERIES_TEAM_SLOTS],
            player_count: 0,
            win_rule: WinRuleState::default(),
            teams: settings.teams,
            launched_step: 0,
            match_end: None,
            ledgers: LedgerBook::default(),
            next_party: 0,
            seat_moves: Vec::new(),
            respawn: settings.respawn,
            respawns: RespawnBook::default(),
            spawn,
            rule_modes: std::collections::BTreeMap::new(),
            waves: CoopWaves::new(settings.waves),
            race: Race::new(settings.race),
            zone: settings.zone,
            zone_state: ZoneState::default(),
            elimination: settings.elimination,
            placements: EliminationBook::default(),
            victory: settings.victory,
            victory_state: VictoryState::default(),
            missing_content: std::collections::BTreeSet::new(),
            winner: None,
            vacated: Vec::new(),
        }
    }

    /// True when a credit to `seat` books: it is seated, or no player ever left it
    /// ([`MatchSession::vacated`]).
    pub fn credits_seat(&self, seat: SeatId) -> bool {
        self.flow.seated.contains(&seat) || self.vacated.binary_search(&seat).is_err()
    }

    /// Update [`MatchSession::vacated`] after a seat change: every seat in `before` that is no
    /// longer seated is vacated, every seated seat is not.
    pub(crate) fn note_seat_changes(&mut self, before: &[SeatId]) {
        for seat in before {
            if !self.flow.seated.contains(seat)
                && let Err(at) = self.vacated.binary_search(seat)
            {
                self.vacated.insert(at, *seat);
            }
        }
        let seated = &self.flow.seated;
        self.vacated.retain(|seat| !seated.contains(seat));
    }

    /// Whether row `row`'s map and mode are installed (neither is in
    /// [`MatchSession::missing_content`]).
    pub fn row_content_present(&self, row: &PlaylistEntry) -> bool {
        content_present(&self.missing_content, row)
    }

    /// The session's settings.
    pub fn settings(&self) -> SessionSettings {
        SessionSettings {
            flow: self.flow.settings,
            objectives: self.board.settings,
            teams: self.teams.clone(),
            respawn: self.respawn.clone(),
            waves: self.waves.settings.clone(),
            race: self.race.settings.clone(),
            zone: self.zone.clone(),
            elimination: self.elimination.clone(),
            victory: self.victory.clone(),
        }
    }

    /// Apply one input as the host ([`Issuer::host`]).
    ///
    /// Same as [`MatchSession::apply_from`] with a host issuer, so existing callers keep working.
    pub fn apply(&mut self, input: &FlowInput) -> Result<(), RulesError> {
        self.apply_from(Issuer::host(), input)
    }

    /// Apply one input from `issuer`. The issuer is not read from `input`.
    ///
    /// A successful [`FlowInput::Seat`] of a seat nobody held opens a zero ledger row with a new
    /// id, replacing whatever a credit to that empty coordinate left there; seating a seat that is
    /// already seated keeps its row. A successful [`FlowInput::Leave`] drops that row. Neither
    /// writes a team score. Both drop the seat's respawn rows ([`RespawnBook::forget`]): a pending
    /// release, a lives count or an elimination never passes to an empty seat or a new occupant.
    /// A leave makes the seat's race row DNF when it has not finished ([`Race::retire`]); a new
    /// occupant drops a row the seat holds without a place ([`Race::forget_unplaced`]).
    pub fn apply_from(&mut self, issuer: Issuer, input: &FlowInput) -> Result<(), RulesError> {
        let fresh = match input {
            FlowInput::Seat { team, slot } => !self.flow.seated.contains(&SeatId::new(*team, *slot)),
            _ => false,
        };
        if let FlowInput::SurrenderVote { team, slot, yes } = input {
            self.flow.apply_from(issuer, input)?;
            return self.cast_surrender(SeatId::new(*team, *slot), *yes);
        }
        let before = self.flow.seated.clone();
        self.flow.apply_from(issuer, input)?;
        self.note_seat_changes(&before);
        match input {
            FlowInput::Seat { team, slot } => {
                let seat = SeatId::new(*team, *slot);
                if fresh {
                    self.ledgers.reopen(seat);
                    // A new player is alive with full lives, whatever the respawn book held for
                    // this coordinate (ADR-8704).
                    self.respawns.forget(seat);
                    // ... and races from the start line, not on a former occupant's DNF row
                    // (ADR-8802).
                    self.race.forget_unplaced(seat);
                } else {
                    self.ledgers.ensure(seat);
                }
            }
            FlowInput::Leave { team, slot } => {
                let seat = SeatId::new(*team, *slot);
                self.ledgers.remove(seat);
                // The pending release, lives and elimination leave with the player (ADR-8704).
                self.respawns.forget(seat);
                // A racer still running is DNF: a seat that left never places and never holds
                // the race open (ADR-8802, rules-genre-pve-racing-review finding 1).
                let tick = self.flow.match_tick;
                let board = &mut self.board;
                self.race.retire(seat, tick, &mut |event| board.push_event(event));
            }
            _ => {}
        }
        Ok(())
    }

    /// Start the next playlist row now, without a ready gate (a host starting a fresh session).
    ///
    /// [`MatchClock::PerMatch`] sets `match_tick` back to 0 before the row starts. [`MatchClock::Session`]
    /// leaves the tick where it is.
    pub fn start_now(&mut self) {
        self.flow.between = false;
        self.flow.paused = false;
        self.flow.paused_by.clear();
        self.flow.paused_by_seat = None;
        self.flow.paused_for_ticks = 0;
        self.flow.ready.clear();
        self.flow.starts_at = None;
        if self.flow.settings.match_clock == MatchClock::PerMatch {
            self.flow.match_tick = 0;
        }
        self.launch_row(self.flow.step);
    }

    /// Run one lockstep step: the flow (start, hold or run), then, when the match ran, the
    /// capture-the-flag and king-of-the-hill rules at the new match tick over `units`.
    ///
    /// With a phase setting on ([`FlowSettings::phases`]) the phase machine runs around the
    /// objective rules: the warm-up, countdown and round-end holds skip them, the end conditions
    /// are evaluated after them in `crate::phases::END_CONDITION_ORDER`, and rounds reset the
    /// board's play state between rounds (`crate::phases`). Every point a dropped flag lies at is
    /// kept; [`MatchSession::step_with`] takes the host's drop check.
    pub fn step(&mut self, units: &[Contestant]) -> SessionStep {
        self.step_with(units, |_| DropCheck::Keep)
    }

    /// [`MatchSession::step`] with the host's drop check for flags lying on the ground (see
    /// [`ObjectiveBoard::ctf_tick_with`]).
    pub fn step_with<F>(&mut self, units: &[Contestant], drop_check: F) -> SessionStep
    where
        F: FnMut([f32; 3]) -> DropCheck,
    {
        let flow = self.flow.step();
        if flow.launched {
            self.launch_row(flow.step);
        }
        if flow.ran {
            let _span = tracing::trace_span!("match_rules_step").entered();
            let before = self.board.events.len();
            let phases = self.flow.settings.phases;
            // One objective tick per step: the phase path runs it inside its play window. (The
            // v06s-final merge ran both rules a second time after this branch: double hill points,
            // and objectives scored during the warm-up.)
            if phases.is_off() {
                self.board.ctf_tick_with(flow.match_tick, units, drop_check);
                self.board.koth_tick(flow.match_tick, units);
                self.board.zones_tick(flow.match_tick, units);
            } else {
                // The phase machine ticks the objectives inside its live phases, with the same
                // drop check (ADR-8619: the drop check and the phases met at this merge).
                self.step_phases(&phases, flow, units, drop_check);
            }
            // Bound seats are credited for the captures and hill points of either path (ADR-8659).
            self.credit_objective_events(before, units);
            // Genre rules after the objectives (ADR-8695): waves, then the race, then their win
            // conditions. Off (empty table, empty course) they do nothing.
            // The director and the race wait through the warm-up, countdown and round-end holds.
            if phases.is_off() || self.flow.phases.phase.is_play() {
                self.step_genres(flow.match_tick, units);
            }
            self.respawns.release(flow.match_tick, &mut self.board.events);
            self.step_genre(flow.match_tick, units);
        }
        let events = self.drain_events();
        self.absorb(&events);
        SessionStep { flow, events }
    }

    /// Store the opaque carry the next booked round will move. Every peer sets the same value.
    pub fn set_carry(&mut self, carry: [u32; SERIES_TEAM_SLOTS]) {
        self.carry = carry;
    }

    /// Book one round. The host may do this. A guest is refused and the series is unchanged,
    /// including when the series is already over.
    pub fn record_round_from(&mut self, issuer: Issuer, round: &RoundResult) -> Result<bool, RulesError> {
        if !issuer.host {
            return Err(RulesError::NotPermitted);
        }
        Ok(self.series.record_round(round))
    }

    /// Close the open vote and return the winning map id. A guest is refused before the vote
    /// state is read, so a closed or missing vote still answers [`RulesError::NotPermitted`].
    pub fn close_vote_from(&mut self, issuer: Issuer, _now_tick: u64) -> Result<Option<String>, RulesError> {
        if !issuer.host {
            return Err(RulesError::NotPermitted);
        }
        let Some(vote) = self.vote.as_ref() else {
            return Err(RulesError::VoteClosed);
        };
        let winner = vote.winner_map().map(str::to_string);
        self.vote = None;
        Ok(winner)
    }

    /// Cast one ballot. A guest may cast only [`voter_id`] of their own seat, and that check
    /// runs before the window. The host may cast any voter id.
    pub fn cast_vote_from(
        &mut self,
        issuer: Issuer,
        voter: u32,
        option: usize,
        now_tick: u64,
    ) -> Result<(), RulesError> {
        if !issuer.host {
            match issuer.seat {
                Some(seat) if voter_id(seat) == voter => {}
                _ => return Err(RulesError::NotPermitted),
            }
        }
        let vote = self.vote.as_mut().ok_or(RulesError::VoteClosed)?;
        match vote.cast_vote(voter, option, now_tick) {
            Ok(()) => Ok(()),
            Err(VoteReject::Closed) => Err(RulesError::VoteClosed),
            Err(VoteReject::BadOption) => Err(RulesError::VoteBadOption { option }),
        }
    }

    /// Run the session's win rule against `view`.
    pub fn evaluate_win(&self, view: &dyn MatchView) -> Option<Outcome> {
        self.win_rule.evaluate(view)
    }

    /// Record a kill on the ledger. Teams outside the ledger cap are ignored.
    pub fn note_kill(&mut self, tick: u64, killer_team: u8, victim_team: u8) {
        self.ledger.note_kill(tick, killer_team, victim_team);
    }

    /// Record a structure loss on the ledger.
    pub fn note_structure(&mut self, tick: u64, team: u8, stem: &str, is_producer: bool, is_primary: bool) {
        self.ledger
            .note_structure(tick, team, stem, is_producer, is_primary);
    }

    fn absorb(&mut self, events: &[RulesEvent]) {
        for event in events {
            if let RulesEvent::MatchOver { tick, team, .. } = event {
                self.winner = Some(*team);
                self.ledger.note_over(*tick);
                let round = RoundResult {
                    winner: *team,
                    carry: self.carry,
                };
                let _ = self.series.record_round(&round);
                self.open_end_vote(*tick);
            }
        }
    }

    fn open_end_vote(&mut self, now_tick: u64) {
        if !self.maps.map_vote {
            return;
        }
        let options = if self.playlist.entries.is_empty() {
            let current = self
                .current
                .as_ref()
                .map(|row| row.entry.map_id.as_str())
                .unwrap_or("");
            self.maps.next_options(current)
        } else {
            let missing = &self.missing_content;
            self.playlist
                .vote_candidates_for(self.maps.option_count, self.player_count, |row| {
                    content_present(missing, row)
                })
                .into_iter()
                .map(|row| row.map_id)
                .collect()
        };
        if options.is_empty() {
            return;
        }
        self.vote = Some(MapVote::open(options, now_tick, self.maps.duration_ticks));
    }

    /// Change the phase settings for the next match (the running match keeps its phase state).
    /// Refused settings change nothing ([`RulesError::PhaseSettings`]).
    pub fn set_phases(&mut self, phases: PhaseSettings) -> Result<(), RulesError> {
        phases.validate()?;
        self.flow.settings.phases = phases;
        Ok(())
    }

    /// Report a condition the host evaluated (a game, series or genre-kit rule): `team` won
    /// (`0` for none) for `reason`. With a phase setting on it lands first in the end-condition
    /// order on the next step the match plays, and ends the round when rounds are on (the match
    /// for [`EndReason::Ended`] and [`EndReason::Forfeit`]). With every phase setting off it ends
    /// the match now, as `ObjectiveBoard::end_match` does.
    pub fn report_condition(&mut self, team: u8, reason: EndReason) {
        if self.flow.settings.phases.is_off() {
            self.board.end_match(self.flow.match_tick, team, reason);
        } else {
            self.flow.phases.report_condition(team, reason);
        }
    }

    /// The `WinCondition` adapter (KI-GP-030, ADR-10070): evaluate `rule` (a stock rule, the
    /// session's own [`Self::win_rule`], or a game's) against `view` and, when it ends the match,
    /// report it through [`Self::report_condition`] with the outcome's winner and an end reason:
    /// [`EndReason::Draw`] when no team won (winner 0), [`EndReason::LastStanding`] for the stock
    /// `lives` rule, [`EndReason::Destroyed`] for `structures`, [`EndReason::Ended`] for any other
    /// rule. Returns the outcome, or `None` when the rule does not end the match or the match is
    /// already over. Call it once per played step, after the step; every peer passes the same view.
    pub fn report_win_condition(&mut self, rule: &dyn WinCondition, view: &dyn MatchView) -> Option<Outcome> {
        if self.board.match_over {
            return None;
        }
        let outcome = rule.evaluate(view)?;
        self.report_condition(outcome.winner, win_end_reason(&outcome));
        Some(outcome)
    }

    /// Add `points` to team `team`'s board score (negative points take them away; the score stays
    /// in `0..=u32::MAX`). A host or game decision such as a logic graph's `rules::AddScore`. It
    /// does not check the capture or hill limit and sends no event: end the match with
    /// [`MatchSession::report_condition`] when a scripted score should win it. Returns the new
    /// score, or `None` (nothing changes) when `team` is not a fighting team or the match is over.
    pub fn add_team_score(&mut self, team: u8, points: i64) -> Option<u32> {
        if !self.board.settings.is_fighting_team(team) || self.board.match_over {
            return None;
        }
        let score = self.board.team_scores.get_mut(usize::from(team))?;
        let next = i64::from(*score)
            .saturating_add(points)
            .clamp(0, i64::from(u32::MAX));
        *score = u32::try_from(next).unwrap_or(u32::MAX);
        Some(*score)
    }

    /// Add `points` to the ledger score of `seat` (negative points take them away) and queue a
    /// `player_scored` event with reason `scripted`. Kills, deaths and objective points do not
    /// change. Returns the new score, or `None` (nothing changes) when nobody holds `seat` or the
    /// match is over. A seat nobody holds is refused for the reason `record_death` ignores it: a
    /// row opened there would be inherited by the next occupant.
    pub fn add_seat_score(&mut self, seat: SeatId, points: i64) -> Option<i64> {
        if !self.flow.seated.contains(&seat) || self.board.match_over {
            return None;
        }
        let tick = self.flow.match_tick;
        let index = self.ledgers.ensure(seat);
        let row = self.ledgers.rows.get_mut(index)?;
        row.score = row.score.saturating_add(points);
        let event = RulesEvent::PlayerScored {
            tick,
            team: row.team,
            slot: row.slot,
            reason: super::objective::ScoreReason::Scripted,
            score: row.score,
        };
        let score = row.score;
        self.board.push_event(event);
        Some(score)
    }

    fn step_phases<F>(&mut self, phases: &PhaseSettings, flow: FlowStep, units: &[Contestant], drop_check: F)
    where
        F: FnMut([f32; 3]) -> DropCheck,
    {
        let seats = self.flow.playing_seats();
        let flag_teams: Vec<u8> = self.board.flag_bases().iter().map(|(team, _)| *team).collect();
        if self.board.match_over && self.flow.phases.phase.is_play() {
            // The host ended the match between steps.
            self.flow
                .phases
                .enter_post_match(phases, flow.step, &mut self.board.events);
        }
        let plan =
            self.flow
                .phases
                .begin_tick(phases, flow.step, &seats, &flag_teams, &mut self.board.events);
        if plan.swapped {
            let bases = self.board.flag_bases();
            if self.flow.phases.side_bases.is_empty() {
                // Recorded under the sides before this swap (the identity on the first swap).
                let mut before = self.flow.phases.clone();
                before.sides.clear();
                before.record_side_bases(&bases);
                self.flow.phases.side_bases = before.side_bases;
            }
            for flag in self.board.objectives.iter_mut().filter(|o| o.is_flag()) {
                if let Some(base) = self.flow.phases.base_of(flag.team_id) {
                    flag.base_position = base;
                }
            }
        }
        if plan.reset_play {
            self.board.reset_play();
            self.respawns.clear();
            self.waves.reset();
            self.race.reset();
            self.zone_state.reset();
            self.placements.clear();
        }
        if plan.open_between {
            self.flow.open_between();
        }
        if !plan.play || self.board.match_over {
            return;
        }
        let events_from = self.board.events.len();
        self.board.ctf_tick_with(flow.match_tick, units, drop_check);
        self.board.koth_tick(flow.match_tick, units);
        self.board.zones_tick(flow.match_tick, units);
        let score_limit = if self.board.match_over {
            self.board.events[events_from..]
                .iter()
                .rev()
                .find_map(|e| match e {
                    RulesEvent::MatchOver { team, reason, .. } => Some((*team, *reason)),
                    _ => None,
                })
        } else {
            None
        };
        let mut events = Vec::new();
        // Attack/defend reads the defenders as the leader at time-up (ADR-8649).
        let time_up_scores = self.board.time_up_scores();
        let end = self.flow.phases.end_tick(
            phases,
            flow.step,
            flow.match_tick,
            &time_up_scores,
            score_limit,
            &mut events,
        );
        // The board's own end stands only when it decided the match: a round end, or an earlier
        // condition in the order, takes it back.
        let board_stands = score_limit.is_some() && end.round.is_none() && end.matched == score_limit;
        if score_limit.is_some() && !board_stands {
            self.board.reopen(events_from);
        }
        self.board.events.extend(events);
        if let Some((team, reason)) = end.matched {
            if !board_stands {
                self.board.end_match(flow.match_tick, team, reason);
            }
            self.flow
                .phases
                .enter_post_match(phases, flow.step, &mut self.board.events);
        }
    }

    /// Report the death of `seat` at the current match tick (ADR-8647). Its team's
    /// [`crate::spawn::RespawnPolicy`] books the respawn ([`RulesEvent::RespawnAt`] on the release
    /// step) or eliminates the seat ([`RulesEvent::Eliminated`], returned by the next step). An
    /// eliminated or already waiting seat is ignored.
    ///
    /// With co-op waves on and a shared lives pool ([`WaveSettings::shared_lives`]), a death on
    /// the players' team spends one life from the pool; with the pool empty the seat is
    /// eliminated whatever its policy.
    pub fn note_death(&mut self, seat: SeatId) {
        let mut policy = self.respawn.policy_of(seat.team);
        if !self.waves.is_off()
            && seat.team == self.waves.settings.players_team
            && !self.respawns.is_eliminated(seat)
            && self.respawns.due_of(seat).is_none()
            && !self.waves.spend_life()
        {
            policy = RespawnPolicy::Never;
        }
        let tick = self.flow.match_tick;
        self.respawns
            .note_death(seat, tick, &policy, &mut self.board.events);
    }

    /// The ask for `team`'s next spawn: its current side ([`crate::phases::PhaseState::side_of`])
    /// and the current match tick. Fill `id`, `tags` and `clearance` before picking.
    pub fn spawn_ask(&self, team: u8) -> SpawnAsk {
        SpawnAsk {
            team,
            side: if team == 0 {
                0
            } else {
                self.flow.phases.side_of(team)
            },
            tick: self.flow.match_tick,
            ..SpawnAsk::default()
        }
    }

    /// Choose a spawn point with the session's [`crate::spawn::SpawnPolicy`] and generator (see
    /// [`crate::spawn::pick_spawn`]). The ask's side and tick are taken from the session.
    pub fn pick_spawn<O>(
        &mut self,
        points: &[SpawnPointData],
        ask: &SpawnAsk,
        units: &[Contestant],
        occluded: O,
    ) -> Option<SpawnChoice>
    where
        O: FnMut([f32; 3], [f32; 3]) -> bool,
    {
        let ask = self.session_ask(ask);
        let policy = self.respawn.spawn;
        super::spawn::pick_spawn(points, &ask, units, &policy, &mut self.spawn, occluded)
    }

    /// Every point's eligibility and score terms for `ask` (see [`crate::spawn::preview_spawn`]),
    /// with the session policy's weights, or the default weights when the policy is not scored.
    pub fn spawn_preview<O>(
        &self,
        points: &[SpawnPointData],
        ask: &SpawnAsk,
        units: &[Contestant],
        occluded: O,
    ) -> Vec<SpawnTerms>
    where
        O: FnMut([f32; 3], [f32; 3]) -> bool,
    {
        let ask = self.session_ask(ask);
        let weights = match self.respawn.spawn {
            super::spawn::SpawnPolicy::Scored(weights) => weights,
            _ => SpawnWeights::default(),
        };
        super::spawn::preview_spawn(points, &ask, units, &weights, &self.spawn, occluded)
    }

    fn session_ask(&self, ask: &SpawnAsk) -> SpawnAsk {
        let base = self.spawn_ask(ask.team);
        SpawnAsk {
            side: base.side,
            tick: base.tick,
            ..ask.clone()
        }
    }

    /// Take the events not yet returned by a step (inputs between steps add some), flow first.
    ///
    /// The first `MatchOver` drained since the launch is kept as [`MatchSession::match_end`].
    pub fn drain_events(&mut self) -> Vec<RulesEvent> {
        let mut events = self.flow.drain_events();
        events.extend(self.board.drain_events());
        if self.match_end.is_none() {
            let step = self.flow.step;
            self.match_end = events.iter().find_map(|event| match *event {
                RulesEvent::MatchOver { tick, team, reason } => Some(MatchEnd {
                    step,
                    tick,
                    winner: team,
                    reason,
                }),
                _ => None,
            });
        }
        events
    }

    fn launch_row(&mut self, step: u64) {
        self.seat_moves.clear();
        if self.teams.rebalance_between_rounds {
            let before = self.flow.seated.clone();
            super::teams::rebalance(self);
            self.note_seat_changes(&before);
        }
        // ADR-8897: rows outside the player gate or with missing content are passed over (one
        // `row_skipped` each, before `launched`); when none fits, the current row reruns.
        let missing = &self.missing_content;
        let pick = self
            .playlist
            .take_next_for(self.player_count, |row| content_present(missing, row));
        for skip in &pick.skipped {
            let map_id = self
                .playlist
                .entries
                .get(skip.index as usize)
                .map(|row| row.map_id.clone())
                .unwrap_or_default();
            self.flow.push_event(RulesEvent::RowSkipped {
                step,
                index: skip.index,
                map_id,
                reason: skip.reason,
            });
        }
        let row = pick.row;
        let mode = match &row {
            Some((_, entry)) => self
                .rule_modes
                .get(&entry.mode)
                .copied()
                .unwrap_or_else(|| entry.match_mode()),
            None => self.board.mode,
        };
        self.board.reset(mode);
        let phases = self.flow.settings.phases;
        let mut phase_events = Vec::new();
        self.flow.phases.start_match(&phases, step, &mut phase_events);
        self.ledgers.reset_match_stats();
        self.respawns.clear();
        self.spawn.last = None;
        self.spawn.uses.clear();
        self.waves.reset();
        self.race.reset();
        self.zone_state.reset();
        self.placements.clear();
        self.victory_state.clear();
        self.launched_step = step;
        self.match_end = None;
        self.flow.push_event(RulesEvent::Launched {
            step,
            index: row.as_ref().map(|(index, _)| *index),
            map_id: row.as_ref().map(|(_, e)| e.map_id.clone()).unwrap_or_default(),
            mode: row.as_ref().map(|(_, e)| e.mode.clone()).unwrap_or_default(),
        });
        for event in phase_events {
            self.flow.push_event(event);
        }
        if let Some((index, entry)) = row {
            self.current = Some(CurrentRow { index, entry });
        }
        self.ledger.begin(self.flow.match_tick);
        self.vote = None;
    }

    /// True while the match plays: always with every phase setting off, otherwise in a play
    /// phase; never once the board is over.
    fn genre_playing(&self) -> bool {
        !self.board.match_over && (self.flow.settings.phases.is_off() || self.flow.phases.phase.is_play())
    }

    /// The zone, the eliminations and the victory conditions on match tick `tick` (module docs of
    /// `crate::victory`). Nothing runs while they are all off.
    fn step_genre(&mut self, tick: u64, units: &[Contestant]) {
        if !self.genre_playing() {
            return;
        }
        if let Some(schedule) = &self.zone {
            self.zone_state
                .tick(schedule, tick, units, &mut self.board.events);
        }
        let seats_rule = self.elimination.last_standing || self.victory.has("team_elimination");
        let any_out = !self.placements.out.is_empty();
        if (!seats_rule && !self.victory.is_on() && !any_out) || self.placements.decided {
            return;
        }
        let seats = self.flow.playing_seats();
        self.placements.note_contenders(&seats);
        if self.victory.has("surrender") {
            let eliminated = &self.respawns.eliminated;
            let passed = self.victory_state.resolve(
                &self.victory.surrender,
                tick,
                |team| voting_slots(&seats, eliminated, team),
                &mut self.board.events,
            );
            self.placements
                .put_out(&passed, tick, OutCause::Surrendered, &mut self.board.events);
        }
        for condition in &self.victory.conditions {
            if let VictoryCondition::DestroyAllTagged { tag } = condition {
                let stripped = self.victory_state.stripped(tag, &self.placements.standing());
                self.placements
                    .put_out(&stripped, tick, OutCause::Destroyed, &mut self.board.events);
            }
        }
        if seats_rule {
            let gone = teams_without_seats(&self.placements, &seats, &self.respawns.eliminated);
            self.placements
                .put_out(&gone, tick, OutCause::Eliminated, &mut self.board.events);
        }
        if let Some((team, winners, reason)) = self.placements.decide(&self.elimination) {
            self.finish_genre(tick, team, winners, reason);
            return;
        }
        let standing = self.placements.standing();
        let mut won = None;
        for condition in &self.victory.conditions {
            won = match condition {
                VictoryCondition::HoldFor { objective, ticks } => self
                    .victory_state
                    .held_long_enough(*objective, *ticks, tick)
                    .filter(|team| standing.contains(team))
                    .map(|team| (team, EndReason::Held)),
                VictoryCondition::ScoreThreshold { score } => self
                    .victory_state
                    .score_leader(*score, &standing)
                    .map(|team| (team, EndReason::ScoreReached)),
                _ => None,
            };
            if won.is_some() {
                break;
            }
        }
        if let Some((team, reason)) = won {
            let side = self.elimination.side_of(team);
            let winners: Vec<u8> = self
                .placements
                .contenders
                .iter()
                .copied()
                .filter(|t| self.elimination.side_of(*t) == side)
                .collect();
            let first = standing
                .iter()
                .copied()
                .find(|t| winners.contains(t))
                .unwrap_or(team);
            self.finish_genre(tick, first, winners, reason);
        }
    }

    fn finish_genre(&mut self, tick: u64, team: u8, winners: Vec<u8>, reason: EndReason) {
        self.placements.decided = true;
        self.placements.winners = winners.clone();
        self.board.events.push(RulesEvent::Victory {
            tick,
            teams: winners,
            reason,
        });
        self.report_condition(team, reason);
    }

    fn cast_surrender(&mut self, seat: SeatId, yes: bool) -> Result<(), RulesError> {
        let open = self.victory.has("surrender")
            && !self.flow.between
            && self.genre_playing()
            && !self.placements.decided
            && !self.placements.is_out(seat.team);
        if !open {
            return Err(RulesError::SurrenderOff);
        }
        if !self.flow.playing_seats().contains(&seat) || self.respawns.is_eliminated(seat) {
            return Err(RulesError::SeatMissing);
        }
        let tick = self.flow.match_tick;
        self.victory_state.cast(seat, yes, tick)?;
        self.board.events.push(RulesEvent::SurrenderVote {
            tick,
            team: seat.team,
            slot: seat.slot,
            yes,
        });
        Ok(())
    }

    /// Report that `team` has `count` units with `tag` (`crate::victory::VictoryCondition::DestroyAllTagged`).
    pub fn report_tagged(&mut self, team: u8, tag: &str, count: u32) -> Result<(), RulesError> {
        self.victory_state.report_tagged(team, tag, count)
    }

    /// Report that `team` (`0` for nobody) holds `objective` now
    /// (`crate::victory::VictoryCondition::HoldFor`). A change of holder restarts the count at the
    /// current match tick.
    pub fn report_holder(&mut self, objective: u32, team: u8) -> Result<(), RulesError> {
        let tick = self.flow.match_tick;
        self.victory_state.report_holder(objective, team, tick)
    }

    /// Report `team`'s score (`crate::victory::VictoryCondition::ScoreThreshold`).
    pub fn report_score(&mut self, team: u8, score: i64) -> Result<(), RulesError> {
        self.victory_state.report_score(team, score)
    }

    /// Put `team` out of the match now (the host's own defeat rule), with a
    /// [`RulesEvent::TeamEliminated`] of cause `host`. The next step decides the match when one
    /// side is left. A team that never played, or is already out, is ignored.
    pub fn defeat_team(&mut self, team: u8) {
        let seats = self.flow.playing_seats();
        self.placements.note_contenders(&seats);
        let tick = self.flow.match_tick;
        self.placements
            .put_out(&[team], tick, OutCause::Host, &mut self.board.events);
    }

    /// Every placed team, best first ([`EliminationBook::placements`]).
    pub fn placements(&self) -> Vec<Placement> {
        self.placements.placements()
    }

    /// Credit bound seats for objective events appended at `events[before..]`.
    ///
    /// A flag capture credits the carrier. A zone capture (ADR-8649) and a hill point credit each
    /// living unit of that team inside the zone or hill, in the host's order, once per unit id: a
    /// unit the host lists twice is credited once (ADR-8657). Unbound units add nothing, and the
    /// team-score vector is not written again.
    fn credit_objective_events(&mut self, before: usize, units: &[Contestant]) {
        let fresh: Vec<RulesEvent> = self.board.events.iter().skip(before).cloned().collect();
        for event in fresh {
            match event {
                RulesEvent::FlagCaptured { tick, unit, .. } => {
                    self.ledgers
                        .credit_objective_unit(unit, tick, &mut self.board.events);
                }
                RulesEvent::ZoneCaptured { tick, zone, team, .. } => {
                    let Some(volume) = self.board.zone(zone).map(|z| z.settings.volume) else {
                        continue;
                    };
                    self.credit_occupants(volume, team, tick, units);
                }
                RulesEvent::HillPoint { tick, team, .. } => {
                    let volume = self
                        .board
                        .objectives
                        .iter()
                        .find(|objective| objective.is_hill())
                        .and_then(|objective| objective.volume);
                    let Some(volume) = volume else {
                        continue;
                    };
                    self.credit_occupants(volume, team, tick, units);
                }
                _ => {}
            }
        }
    }

    /// Credit one objective point to each living unit of `team` inside `volume`, in the host's
    /// order. A unit id the host lists twice is credited once.
    fn credit_occupants(&mut self, volume: HillVolume, team: u8, tick: u64, units: &[Contestant]) {
        let mut credited: Vec<u32> = Vec::new();
        for unit in units {
            if !(unit.alive && unit.team == team && volume.contains(unit.position)) {
                continue;
            }
            if let Err(at) = credited.binary_search(&unit.id) {
                credited.insert(at, unit.id);
                self.ledgers
                    .credit_objective_unit(unit.id, tick, &mut self.board.events);
            }
        }
    }
}

impl MatchSession {
    /// Replace the co-op wave settings and clear the director (refused settings change nothing).
    pub fn set_waves(&mut self, settings: WaveSettings) -> Result<(), RulesError> {
        settings.validate()?;
        self.waves = CoopWaves::new(settings);
        Ok(())
    }

    /// Replace the race settings and clear the race (refused settings change nothing).
    pub fn set_race(&mut self, settings: RaceSettings) -> Result<(), RulesError> {
        settings.validate()?;
        self.race = Race::new(settings);
        Ok(())
    }

    /// `count` enemies of wave `wave` died (the game tags each spawned enemy with its wave).
    pub fn note_wave_down(&mut self, wave: u32, count: u32) {
        self.waves.note_down(wave, count);
    }

    /// The host's co-op objective report: destroyed fails the match, reached wins it when
    /// [`crate::waves::CoopSuccess`] counts it. Evaluated on the next step.
    pub fn note_coop_objective(&mut self, objective: CoopObjective) {
        self.waves.note_objective(objective);
    }

    /// Enter `seat` driving `unit` in the race. A seated seat whose ledger row is bound to a unit
    /// ([`crate::teams::SeatRequest::unit`]) is entered on its own on the next step.
    pub fn race_enter(&mut self, seat: SeatId, unit: u32) {
        self.race.enter(seat, unit);
    }

    /// The race's live standings ([`Race::standings`]).
    pub fn race_standings(&self) -> Vec<Standing> {
        self.race.standings()
    }

    /// Waves, the race, then their win conditions, at `tick` (ADR-8695).
    fn step_genres(&mut self, tick: u64, units: &[Contestant]) {
        if self.board.match_over {
            return;
        }
        let decided = self.waves.outcome().is_some() || self.race.outcome().is_some();
        if !self.waves.is_off() {
            let team = self.waves.settings.players_team;
            let mut players = 0u32;
            let mut down = 0u32;
            for seat in self.flow.playing_seats() {
                if seat.team != team {
                    continue;
                }
                players = players.saturating_add(1);
                // Down: eliminated, or waiting past this tick (an instant respawn is released
                // on this step, so it never counts toward a wipe).
                if self.respawns.is_eliminated(seat)
                    || self.respawns.due_of(seat).is_some_and(|due| due > tick)
                {
                    down = down.saturating_add(1);
                }
            }
            let view = CoopView {
                tick,
                players,
                players_down: down,
            };
            let board = &mut self.board;
            self.waves.step(view, &mut |event| board.push_event(event));
        }
        if !self.race.is_off() {
            for seat in self.flow.playing_seats() {
                if let Some(unit) = self.ledgers.row(seat).and_then(|row| row.unit) {
                    self.race.enter(seat, unit);
                }
            }
            let mut fresh = Vec::new();
            self.race.step(tick, units, &mut |event| fresh.push(event));
            for event in &fresh {
                if let RulesEvent::RacerLap {
                    team,
                    slot,
                    lap_ticks,
                    best: true,
                    ..
                } = event
                {
                    self.record_best_lap(SeatId::new(*team, *slot), *lap_ticks);
                }
            }
            for event in fresh {
                self.board.push_event(event);
            }
        }
        if decided {
            return;
        }
        if let Some(outcome) = self.waves.outcome().or_else(|| self.race.outcome()) {
            self.report_condition(outcome.winner, EndReason::Ended);
        }
    }

    /// Write a best lap into the seat's ledger counter [`BEST_LAP_COUNTER`]. A row already at its
    /// named-counter cap keeps the lap only in the race state.
    fn record_best_lap(&mut self, seat: SeatId, ticks: u64) {
        let new = i64::try_from(ticks).unwrap_or(i64::MAX);
        let old = self
            .ledgers
            .row(seat)
            .and_then(|row| row.counters.get(BEST_LAP_COUNTER).copied());
        let delta = old.map_or(new, |old| new.saturating_sub(old));
        // A row at its counter cap (or an unseated racer) keeps the lap only in the race state.
        let _ = self.ledgers.add_counter(seat, BEST_LAP_COUNTER, delta);
    }
}

fn content_present(missing: &std::collections::BTreeSet<String>, row: &PlaylistEntry) -> bool {
    !missing.contains(&row.map_id) && !missing.contains(&row.mode)
}

impl Default for MatchSession {
    fn default() -> Self {
        Self::new(SessionSettings::default())
    }
}

/// The end reason [`MatchSession::report_win_condition`] reports for `outcome`.
fn win_end_reason(outcome: &Outcome) -> EndReason {
    match (outcome.winner, outcome.rule) {
        (0, _) => EndReason::Draw,
        (_, "lives") => EndReason::LastStanding,
        (_, "structures") => EndReason::Destroyed,
        _ => EndReason::Ended,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objective::HillVolume;

    /// KI-GP-030 (ADR-10070): a `WinCondition` ends the match through `report_condition`, once,
    /// with the rule's end reason; a rule that does not end it changes nothing.
    #[test]
    fn a_win_condition_ends_the_match_through_the_adapter() {
        use crate::win_condition::{LivesOut, RosterView, SeatLives};

        let seats = |lives_2: u32| RosterView {
            seats: vec![
                SeatLives {
                    team: 1,
                    slot: 0,
                    lives: 3,
                },
                SeatLives {
                    team: 2,
                    slot: 0,
                    lives: lives_2,
                },
            ],
            teams: vec![1, 2],
            ..RosterView::default()
        };
        let mut s = session(0);
        assert_eq!(s.report_win_condition(&LivesOut, &seats(1)), None);
        assert!(!s.board.match_over, "both teams in: the match goes on");
        let outcome = s
            .report_win_condition(&LivesOut, &seats(0))
            .expect("team 2 is out");
        assert_eq!((outcome.winner, outcome.rule), (1, "lives"));
        assert!(s.board.match_over);
        assert_eq!(s.board.end_reason, Some(EndReason::LastStanding));
        let over: Vec<RulesEvent> = s
            .drain_events()
            .into_iter()
            .filter(|e| matches!(e, RulesEvent::MatchOver { .. }))
            .collect();
        assert!(
            matches!(
                over.as_slice(),
                [RulesEvent::MatchOver {
                    team: 1,
                    reason: EndReason::LastStanding,
                    ..
                }]
            ),
            "{over:?}"
        );
        assert_eq!(
            s.report_win_condition(&LivesOut, &seats(0)),
            None,
            "an ended match is left alone"
        );
        assert_eq!(
            win_end_reason(&Outcome {
                winner: 0,
                rule: "lives"
            }),
            EndReason::Draw
        );
        assert_eq!(
            win_end_reason(&Outcome {
                winner: 2,
                rule: "structures"
            }),
            EndReason::Destroyed
        );
        assert_eq!(
            win_end_reason(&Outcome {
                winner: 2,
                rule: "flags"
            }),
            EndReason::Ended
        );
    }

    fn session(delay: u64) -> MatchSession {
        MatchSession::new(SessionSettings {
            flow: FlowSettings {
                start_delay_ticks: delay,
                ..FlowSettings::default()
            },
            ..SessionSettings::default()
        })
    }

    #[test]
    fn playlist_waits_for_every_ready_then_starts_each_row_in_turn() {
        let mut s = session(0);
        s.playlist.set(vec![
            PlaylistEntry::new("cafe", "tdm"),
            PlaylistEntry::new("hq", "search"),
        ]);
        s.apply(&FlowInput::Seat { team: 1, slot: 0 }).expect("seat");
        s.apply(&FlowInput::Seat { team: 2, slot: 0 }).expect("seat");
        s.apply(&FlowInput::OpenBetween).expect("between");
        let frozen = s.flow.match_tick;
        s.step(&[]);
        assert_eq!(s.flow.match_tick, frozen);
        s.apply(&FlowInput::Ready { team: 1, slot: 0 }).expect("ready");
        assert!(!s.step(&[]).flow.launched);
        s.apply(&FlowInput::Ready { team: 2, slot: 0 }).expect("ready");
        assert!(s.step(&[]).flow.launched);
        let row = s.current.clone().expect("row");
        assert_eq!(
            (row.index, row.entry.map_id.as_str(), row.entry.mode.as_str()),
            (0, "cafe", "tdm")
        );
        assert_eq!(s.board.mode, MatchMode::TeamDeathmatch);
        s.apply(&FlowInput::OpenBetween).expect("between");
        s.apply(&FlowInput::Ready { team: 1, slot: 0 }).expect("ready");
        s.apply(&FlowInput::Ready { team: 2, slot: 0 }).expect("ready");
        let step = s.step(&[]);
        assert!(step.flow.launched);
        assert!(step.events.iter().any(|e| matches!(
            e,
            RulesEvent::Launched { index: Some(1), map_id, mode, .. } if map_id == "hq" && mode == "search"
        )));
        assert_eq!(
            s.board.mode,
            MatchMode::Custom,
            "a game's own mode runs as custom"
        );
    }

    #[test]
    fn a_launch_resets_scores_and_objectives_for_the_rows_mode() {
        let mut s = session(0);
        s.playlist.set(vec![
            PlaylistEntry::new("a", "koth"),
            PlaylistEntry::new("b", "ctf"),
        ]);
        s.start_now();
        assert_eq!(s.board.mode, MatchMode::KingOfTheHill);
        s.board
            .install_hill_volume(
                77,
                HillVolume {
                    min: [-1.0, -1.0, -1.0],
                    max: [1.0, 1.0, 1.0],
                },
            )
            .expect("hill");
        let inside = [Contestant {
            id: 5,
            team: 2,
            position: [0.0, 0.0, 0.0],
            alive: true,
        }];
        for _ in 0..60 {
            s.step(&inside);
        }
        assert_eq!(s.board.score(2), 1);
        s.apply(&FlowInput::Seat { team: 2, slot: 0 }).expect("seat");
        s.apply(&FlowInput::OpenBetween).expect("between");
        s.apply(&FlowInput::Ready { team: 2, slot: 0 }).expect("ready");
        s.step(&inside);
        assert_eq!(s.board.mode, MatchMode::CaptureTheFlag);
        assert_eq!(s.board.score(2), 0);
        assert!(
            s.board.objectives.is_empty(),
            "the game places the new map's objectives"
        );
    }

    #[test]
    fn nothing_scores_while_paused() {
        let mut s = session(0);
        s.playlist.set(vec![PlaylistEntry::new("a", "koth")]);
        s.start_now();
        s.board
            .install_hill_volume(
                1,
                HillVolume {
                    min: [-1.0, -1.0, -1.0],
                    max: [1.0, 1.0, 1.0],
                },
            )
            .expect("hill");
        let inside = [Contestant {
            id: 5,
            team: 1,
            position: [0.0, 0.0, 0.0],
            alive: true,
        }];
        s.apply(&FlowInput::Pause { name: "ada".into() }).expect("pause");
        for _ in 0..600 {
            s.step(&inside);
        }
        assert_eq!(s.board.score(1), 0);
        assert_eq!(s.flow.match_tick, 0);
        s.apply(&FlowInput::Resume {
            name: "ada".into(),
            host: false,
        })
        .expect("resume");
        for _ in 0..60 {
            s.step(&inside);
        }
        assert_eq!(s.board.score(1), 1);
    }

    #[test]
    fn a_session_round_trips_through_serde() {
        let mut s = session(5);
        s.playlist.set(vec![PlaylistEntry::new("a", "ctf")]);
        s.apply(&FlowInput::Seat { team: 1, slot: 3 }).expect("seat");
        s.start_now();
        s.board.spawn_flag(9, 1, [1.0, 0.0, 2.0]).expect("flag");
        s.drain_events();
        let text = serde_json::to_string(&s).expect("serialize");
        let back: MatchSession = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, s);
    }

    /// R-04 (1), ADR-8881: a serialized session names its shape version, a key this build does
    /// not know is refused (not dropped), a newer or zero version is refused by name, and a
    /// session written before the version key (v0.6) reads as version 1.
    #[test]
    fn a_session_names_its_version_and_refuses_unknown_keys() {
        let mut s = session(5);
        s.playlist.set(vec![PlaylistEntry::new("a", "ctf")]);
        s.apply(&FlowInput::Seat { team: 1, slot: 0 }).expect("seat");
        let value = serde_json::to_value(&s).expect("serialize");
        assert_eq!(
            value.get("version").and_then(serde_json::Value::as_u64),
            Some(u64::from(MATCH_SESSION_VERSION)),
            "the version key is written"
        );
        let mut unknown = value.clone();
        unknown["seat_movez"] = serde_json::json!([]);
        let error = serde_json::from_value::<MatchSession>(unknown)
            .expect_err("an unknown key is refused")
            .to_string();
        assert!(error.contains("seat_movez"), "{error}");
        let mut newer = value.clone();
        newer["version"] = serde_json::json!(MATCH_SESSION_VERSION + 1);
        let error = serde_json::from_value::<MatchSession>(newer)
            .expect_err("a newer version is refused")
            .to_string();
        assert!(error.contains("match_session_version"), "{error}");
        let mut zero = value.clone();
        zero["version"] = serde_json::json!(0);
        assert!(
            serde_json::from_value::<MatchSession>(zero).is_err(),
            "version 0 is refused"
        );
        let mut old = value;
        if let serde_json::Value::Object(map) = &mut old {
            map.remove("version");
        }
        let back: MatchSession = serde_json::from_value(old).expect("a v0.6 session reads");
        assert_eq!(back.version, MATCH_SESSION_VERSION);
        assert_eq!(back, s);
    }

    /// R-04 (2), ADR-8881: a credit to a coordinate a player left (or moved off) opens no ledger
    /// row and books nothing there; the seated side of the same kill is still credited. A
    /// coordinate nobody ever held (a game's unseated bots) is still credited, as before.
    #[test]
    fn credits_to_a_vacated_coordinate_open_no_row() {
        let mut s = session(0);
        let stay = SeatId::new(1, 0);
        let gone = SeatId::new(2, 0);
        let bot = SeatId::new(3, 0);
        for seat in [stay, gone] {
            s.apply(&FlowInput::Seat {
                team: seat.team,
                slot: seat.slot,
            })
            .expect("seat");
        }
        s.apply(&FlowInput::Leave {
            team: gone.team,
            slot: gone.slot,
        })
        .expect("leave");
        assert!(s.ledgers.row(gone).is_none(), "a leave drops the row");
        // The departed player's pawn still kills, and is killed, and assists.
        s.note_hit(stay, gone);
        s.credit_kill(gone, stay);
        s.credit_kill(stay, gone);
        assert!(
            s.ledgers.row(gone).is_none(),
            "no row reopens on the vacated coordinate"
        );
        let row = s.ledgers.row(stay).expect("seated row");
        assert_eq!((row.kills, row.deaths, row.assists), (1, 1, 0));
        // A coordinate nobody ever held keeps S-14's behavior: the credit opens its row.
        s.credit_kill(bot, stay);
        assert_eq!(s.ledgers.row(bot).map(|row| row.kills), Some(1));
        // Seated again, the coordinate is credited again, from a zero row.
        s.apply(&FlowInput::Seat {
            team: gone.team,
            slot: gone.slot,
        })
        .expect("reseat");
        s.credit_kill(gone, stay);
        assert_eq!(s.ledgers.row(gone).map(|row| row.kills), Some(1));
        assert!(s.vacated.is_empty());
        // The vacated list round-trips, so a restored session keeps refusing.
        s.apply(&FlowInput::Leave {
            team: gone.team,
            slot: gone.slot,
        })
        .expect("leave again");
        let text = serde_json::to_string(&s).expect("serialize");
        let mut back: MatchSession = serde_json::from_str(&text).expect("deserialize");
        back.credit_kill(gone, stay);
        assert!(back.ledgers.row(gone).is_none());
    }

    /// R-04 (2): the coordinate a team change leaves is vacated too.
    #[test]
    fn a_team_move_vacates_the_coordinate_it_leaves() {
        use crate::teams::{SeatRequest, TeamSwitch};
        let mut s = session(0);
        s.teams.switch = TeamSwitch::BetweenMatches;
        let from = SeatId::new(1, 0);
        let other = SeatId::new(2, 0);
        for seat in [from, other] {
            s.apply(&FlowInput::Seat {
                team: seat.team,
                slot: seat.slot,
            })
            .expect("seat");
        }
        s.apply(&FlowInput::OpenBetween).expect("between");
        let request = SeatRequest {
            from: Some(from),
            team: Some(3),
            slot: Some(0),
            unit: None,
        };
        assert_eq!(
            s.assign_team(&request, &[]).expect("move"),
            vec![SeatId::new(3, 0)]
        );
        s.credit_kill(from, other);
        assert!(s.ledgers.row(from).is_none(), "the old coordinate opens no row");
        assert_eq!(s.vacated, vec![from]);
    }

    fn koth_at_period(clock: MatchClock, period: u32) -> (MatchSession, Contestant) {
        let mut session = MatchSession::new(SessionSettings {
            flow: FlowSettings {
                start_delay_ticks: 0,
                match_clock: clock,
                ..FlowSettings::default()
            },
            objectives: ObjectiveSettings {
                hill_accrual_ticks: period,
                ..ObjectiveSettings::default()
            },
            ..SessionSettings::default()
        });
        session.playlist.set(vec![
            PlaylistEntry::new("hill-a", "koth"),
            PlaylistEntry::new("hill-b", "koth"),
        ]);
        session.start_now();
        session
            .board
            .install_hill_volume(
                7,
                HillVolume {
                    min: [-1.0, -1.0, -1.0],
                    max: [1.0, 1.0, 1.0],
                },
            )
            .expect("hill");
        let holder = Contestant {
            id: 5,
            team: 1,
            position: [0.0, 0.0, 0.0],
            alive: true,
        };
        while session.flow.match_tick + 1 < u64::from(period) {
            session.step(std::slice::from_ref(&holder));
        }
        assert_eq!(session.flow.match_tick, u64::from(period) - 1);
        assert_eq!(session.board.score(1), 0);
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        session.apply(&FlowInput::OpenBetween).expect("between");
        session
            .apply(&FlowInput::Ready { team: 1, slot: 0 })
            .expect("ready");
        (session, holder)
    }

    /// The launching step is scored on the hill that is already installed. `MatchSession::step`
    /// would reset the board before `koth_tick`, so this calls the flow and the hill directly.
    #[test]
    fn per_match_clock_gives_koth_a_full_first_period() {
        const PERIOD: u32 = 4;
        let (mut session_clock, holder) = koth_at_period(MatchClock::Session, PERIOD);
        let (mut per_match, _) = koth_at_period(MatchClock::PerMatch, PERIOD);
        let units = [holder];

        let launched = session_clock.flow.step();
        assert!(launched.launched);
        assert_eq!(launched.match_tick, u64::from(PERIOD));
        session_clock.board.koth_tick(launched.match_tick, &units);
        assert_eq!(
            session_clock.board.score(1),
            1,
            "the session clock is one tick from the boundary"
        );

        let launched = per_match.flow.step();
        assert!(launched.launched);
        assert_eq!(launched.match_tick, 1);
        per_match.board.koth_tick(launched.match_tick, &units);
        assert_eq!(per_match.board.score(1), 0, "a fresh match waits a full period");

        for _ in 0..PERIOD - 1 {
            let report = per_match.step(&units);
            assert!(!report.flow.launched);
            if report.flow.match_tick == u64::from(PERIOD) {
                assert_eq!(per_match.board.score(1), 1);
            } else {
                assert_eq!(per_match.board.score(1), 0);
            }
        }
        assert_eq!(per_match.flow.match_tick, u64::from(PERIOD));
        assert_eq!(per_match.board.score(1), 1);
    }

    #[test]
    fn vote_and_series_follow_the_issuer() {
        use crate::objective::EndReason;

        let guest = Issuer::guest(SeatId::new(1, 0));
        let mut session = MatchSession {
            series: SeriesState::new(3),
            ..Default::default()
        };
        let round = RoundResult {
            winner: 1,
            carry: [0; SERIES_TEAM_SLOTS],
        };
        let err = session.record_round_from(guest, &round).expect_err("guest");
        assert_eq!(err, RulesError::NotPermitted);
        assert_eq!(session.series.played, 0);
        let err = session.close_vote_from(guest, 0).expect_err("guest close");
        assert_eq!(err, RulesError::NotPermitted);
        assert!(session.vote.is_none());

        assert!(session.record_round_from(Issuer::host(), &round).expect("host"));
        assert_eq!(session.series.played, 1);
        assert_eq!(session.series.wins[1], 1);

        session.maps.map_vote = true;
        session.maps.rotation = vec!["mesa".into(), "canyon".into(), "ridge".into(), "flats".into()];
        let mut carry = [0; SERIES_TEAM_SLOTS];
        carry[2] = 9;
        session.set_carry(carry);
        session.board.end_match(0, 2, EndReason::Ended);
        session.step(&[]);
        assert_eq!(session.series.played, 2, "match over books the round");
        assert_eq!(session.series.wins[2], 1);
        assert_eq!(session.series.carry[2], 9);
        let vote = session.vote.as_ref().expect("vote opened");
        assert_eq!(vote.options, vec!["mesa", "canyon", "ridge"]);

        session.vote.as_mut().expect("vote").close_tick = 0;
        let err = session.close_vote_from(guest, 0).expect_err("guest");
        assert_eq!(err, RulesError::NotPermitted);
        assert!(session.vote.is_some(), "a refused close leaves the vote");
        assert_eq!(
            session.close_vote_from(Issuer::host(), 0).expect("host"),
            Some("mesa".into())
        );
        assert!(session.vote.is_none());
        assert_eq!(
            session.close_vote_from(Issuer::host(), 0).expect_err("none"),
            RulesError::VoteClosed
        );

        session.vote = Some(MapVote::open(vec!["mesa".into(), "canyon".into()], 0, 1));
        session.vote.as_mut().expect("reopened").close_tick = 0;
        let err = session.cast_vote_from(guest, 1, 0, 0).expect_err("other voter");
        assert_eq!(err, RulesError::NotPermitted);
        assert_eq!(session.vote.as_ref().expect("stays").vote_count(), 0);
        let err = session
            .cast_vote_from(guest, voter_id(SeatId::new(1, 0)), 0, 0)
            .expect_err("closed");
        assert_eq!(err, RulesError::VoteClosed);
    }

    /// Review proof (rules-spawn-selection-review). A seat that is eliminated, leaves, and is
    /// sat again is a new occupant: the old elimination must not stick to the coordinate.
    #[test]
    fn proof_review_new_occupant_does_not_inherit_elimination() {
        use crate::spawn::RespawnPolicy;

        let mut session = MatchSession::new(SessionSettings {
            respawn: RespawnRules {
                default: RespawnPolicy::Never,
                ..RespawnRules::default()
            },
            ..SessionSettings::default()
        });
        let seat = SeatId::new(1, 0);
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        session.note_death(seat);
        assert!(session.respawns.is_eliminated(seat));
        session
            .apply(&FlowInput::Leave { team: 1, slot: 0 })
            .expect("leave");
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("reseat");
        assert!(
            !session.respawns.is_eliminated(seat),
            "seat (1, 0) left and was sat again; the new occupant is already eliminated"
        );
        let before = session.board.events.len();
        session.note_death(seat);
        assert!(
            session.board.events[before..]
                .iter()
                .any(|event| matches!(event, RulesEvent::Eliminated { team: 1, slot: 0, .. })),
            "the new occupant's death was ignored because the old elimination remained"
        );
    }

    /// Review proof. A delay respawn booked for a seat is dropped when that seat leaves, so the
    /// next occupant is not released on the previous player's timer.
    #[test]
    fn proof_review_leave_drops_the_pending_respawn() {
        use crate::spawn::RespawnPolicy;

        let mut session = MatchSession::new(SessionSettings {
            respawn: RespawnRules {
                default: RespawnPolicy::Delay { ticks: 40 },
                ..RespawnRules::default()
            },
            ..SessionSettings::default()
        });
        let seat = SeatId::new(1, 0);
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        session.flow.match_tick = 3;
        session.note_death(seat);
        assert_eq!(session.respawns.due_of(seat), Some(43));
        session
            .apply(&FlowInput::Leave { team: 1, slot: 0 })
            .expect("leave");
        assert!(
            session.respawns.due_of(seat).is_none(),
            "leave left a respawn booked for empty seat (1, 0) at tick 43"
        );
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("reseat");
        assert!(
            session.respawns.due_of(seat).is_none(),
            "the new occupant of (1, 0) is already waiting to respawn at tick 43"
        );
    }

    /// Review proof. Moving a player to another team (between matches) must take the pending
    /// respawn with them. Today the book stays on the old coordinate.
    #[test]
    fn proof_review_team_move_carries_the_pending_respawn() {
        use crate::spawn::RespawnPolicy;
        use crate::teams::{SeatRequest, TeamSwitch};

        let mut session = MatchSession::new(SessionSettings {
            respawn: RespawnRules {
                default: RespawnPolicy::Delay { ticks: 40 },
                ..RespawnRules::default()
            },
            ..SessionSettings::default()
        });
        session.teams.switch = TeamSwitch::BetweenMatches;
        let from = SeatId::new(1, 0);
        let dest = SeatId::new(2, 0);
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        session.flow.match_tick = 7;
        session.note_death(from);
        assert_eq!(session.respawns.due_of(from), Some(47));
        session.apply(&FlowInput::OpenBetween).expect("between");
        let moved = session
            .assign_team(
                &SeatRequest {
                    from: Some(from),
                    team: Some(2),
                    slot: Some(0),
                    unit: None,
                },
                &[],
            )
            .expect("move");
        assert_eq!(moved, vec![dest]);
        assert!(
            session.respawns.due_of(from).is_none(),
            "the pending respawn stayed on seat (1, 0) after the player moved to (2, 0)"
        );
        assert_eq!(
            session.respawns.due_of(dest),
            Some(47),
            "the pending respawn did not follow the player to seat (2, 0)"
        );
    }

    /// ADR-8704: a respawn row booked for a coordinate nobody sits on (a pawn tagged with an
    /// unseated seat died) does not pass to whoever is seated there next, through
    /// `FlowInput::Seat` or through `assign_team` without `from`.
    #[test]
    fn a_fresh_seat_starts_with_no_respawn_rows() {
        use crate::spawn::RespawnPolicy;
        use crate::teams::SeatRequest;

        let mut session = MatchSession::new(SessionSettings {
            respawn: RespawnRules {
                default: RespawnPolicy::Delay { ticks: 40 },
                ..RespawnRules::default()
            },
            ..SessionSettings::default()
        });
        let (a, b) = (SeatId::new(1, 0), SeatId::new(2, 0));
        session.note_death(a);
        session.note_death(b);
        assert_eq!(session.respawns.due_of(a), Some(40));
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        assert_eq!(session.respawns.due_of(a), None, "Seat kept a stale booking");
        let placed = session
            .assign_team(
                &SeatRequest {
                    from: None,
                    team: Some(2),
                    slot: Some(0),
                    unit: None,
                },
                &[],
            )
            .expect("assign");
        assert_eq!(placed, vec![b]);
        assert_eq!(
            session.respawns.due_of(b),
            None,
            "assign_team kept a stale booking"
        );
        // Seating a seat that is already seated keeps the player's own booking.
        session.note_death(a);
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat again");
        assert_eq!(session.respawns.due_of(a), Some(40));
    }
}
