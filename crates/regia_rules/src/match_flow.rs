// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/match_flow.rs, commit d6ed37677), ADR-8012.
// Mark: adapt (lines 12-295, the pause, ready, observer and join ledger, and the tests 476-601).
// Named issues:
// - a lockstep start: when the last seat readies, the next match starts on an agreed step
//   (`starts_at`, the ready step plus `start_delay_ticks`) that every peer computes from the same
//   inputs, instead of launching inside the input handler; an unready during the countdown calls it
//   off;
// - the seat ledgers are kept in seat order, so one step's inputs give the same state in any
//   arrival order;
// - the flow owns its clocks: the lockstep step always advances, the match tick freezes under a
//   pause, and the held steps count the friendly-pause hold (60 sim seconds by default);
// - the four-team range, the seats per team, the human seat limit, the hold, the countdown and the
//   pause-name length are settings; errors carry stable codes instead of the game's string table;
// - the commander rules are stripped: any seat of the asking team may be approved for a join, and
//   the game's roster kinds, hulls and AI fill stay in the game;
// - HUD side effects became `RulesEvent`s.
// The game's alpha playlist, map list, mode-name mapping, economy presets and battle scars are not
// carried.
// Issuer checks, leave, the ordered join queue, the per-seat pause budget and the per-match clock
// are Regia's (ADR-8332). They are not part of the copied ledger.

//! Pause, ready-up, observers and join requests for the match the host is running.
//!
//! A [`MatchFlow`] is stepped once per lockstep step ([`MatchFlow::step`]); inputs ([`FlowInput`])
//! land on the step they are applied before. Every peer applies the same inputs on the same steps,
//! so every peer pauses, resumes and starts the next match on the same step.

use serde::{Deserialize, Serialize};

use super::RulesError;
use super::objective::{DEFAULT_MAX_PENDING_EVENTS, DEFAULT_MAX_TEAMS, RulesEvent, push_capped};
use super::phases::{PhaseSettings, PhaseState};

/// Held ticks anyone but the pauser (or the host) waits before they may resume a friendly match:
/// 60 seconds at the default tick rate, on the sim clock, never the wall clock.
pub const DEFAULT_FRIENDLY_UNPAUSE_TICKS: u64 = regia_detsim::tick::ticks_from_secs(60.0);

/// Default countdown from the last ready to the start of the next match: 3 seconds at the default
/// tick rate.
pub const DEFAULT_START_DELAY_TICKS: u64 = regia_detsim::tick::ticks_from_secs(3.0);

/// Default seats per team.
pub const DEFAULT_SEATS_PER_TEAM: u8 = 16;

/// Default longest name on the pause banner, in characters.
pub const DEFAULT_PAUSE_NAME_MAX: u8 = 24;

/// Default join asks kept at once. A further ask is refused.
pub const DEFAULT_MAX_JOIN_ASKS: u8 = 8;

fn default_max_join_asks() -> u8 {
    DEFAULT_MAX_JOIN_ASKS
}

/// Whether `match_tick` keeps running across launches or starts again at 0.
///
/// [`MatchClock::Session`] is the default and the compat value: the tick is not reset on launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchClock {
    /// The match tick continues across launches. King of the hill keeps the previous period phase.
    #[default]
    Session,
    /// `match_tick` is set to 0 when a match launches, so the first accrual waits a full period.
    PerMatch,
}

/// One player seat: a fighting team (`1..=max_teams`) and a slot on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct SeatId {
    /// The team.
    pub team: u8,
    /// The slot on the team.
    pub slot: u8,
}

impl SeatId {
    /// The seat `slot` of `team`.
    pub const fn new(team: u8, slot: u8) -> Self {
        Self { team, slot }
    }
}

/// Banner text for `seat` when a pause is given no name. It is display-only and is never the
/// word `host`.
#[must_use]
pub fn seat_label(seat: SeatId) -> String {
    format!("team {} slot {}", seat.team, seat.slot)
}

/// Who sent a [`FlowInput`].
///
/// The session layer fills this from the lockstep seat map or the server connection. It is never
/// decoded from the input bytes. [`MatchFlow::apply`] and `crate::session::MatchSession::apply`
/// use [`Issuer::host`], so a host-side caller keeps the previous call shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Issuer {
    /// The seat this issuer acts as, when they have one.
    pub seat: Option<SeatId>,
    /// The host may send any input.
    pub host: bool,
}

impl Issuer {
    /// The host, with no seat.
    #[must_use]
    pub const fn host() -> Self {
        Self {
            seat: None,
            host: true,
        }
    }

    /// A guest on `seat`.
    #[must_use]
    pub const fn guest(seat: SeatId) -> Self {
        Self {
            seat: Some(seat),
            host: false,
        }
    }
}

/// An observer asking to join the match, waiting on the host's approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinAsk {
    /// The observer's seat.
    pub seat: SeatId,
    /// The lockstep step the ask was made on. Older snapshots that omit it decode as step 0.
    #[serde(default)]
    pub step: u64,
}

/// Pauses and held ticks one seat has used. Kept in seat order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatPauseUse {
    /// The seat.
    pub seat: SeatId,
    /// Pauses this seat has started.
    pub pauses: u8,
    /// Held ticks charged to this seat.
    pub ticks: u64,
}

/// The flow's settings (caps a game used to hard-code). Every peer of a lockstep match runs the same
/// settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FlowSettings {
    /// Fighting teams are `1..=max_teams`.
    pub max_teams: u8,
    /// Slots per team: a seat's slot is `0..seats_per_team`.
    pub seats_per_team: u8,
    /// Slots from this one up are refused a seat (for example a human cap per side while the rest
    /// of the slots hold AI). `None`: every slot may be seated.
    pub seat_slot_limit: Option<u8>,
    /// Held ticks before anyone may resume a friendly pause.
    pub friendly_unpause_ticks: u64,
    /// Steps from the last ready (or an AFK override) to the start of the next match. `0` starts it
    /// on the step the last ready lands on.
    pub start_delay_ticks: u64,
    /// Longest name on the pause banner, in characters.
    pub pause_name_max: u8,
    /// Join asks kept at once. A further ask is refused. Default [`DEFAULT_MAX_JOIN_ASKS`].
    #[serde(default = "default_max_join_asks")]
    pub max_join_asks: u8,
    /// Pauses one seat may start. `None` (the default) does not limit them.
    #[serde(default)]
    pub max_pauses_per_seat: Option<u8>,
    /// Held ticks one seat may spend paused. `None` (the default) does not limit them.
    #[serde(default)]
    pub max_pause_ticks_per_seat: Option<u64>,
    /// Whether `match_tick` continues across launches ([`MatchClock::Session`], the default) or
    /// starts at 0 ([`MatchClock::PerMatch`]).
    #[serde(default)]
    pub match_clock: MatchClock,
    /// Warm-up, countdown, time limit, overtime, rounds, intermission and forfeit
    /// ([`crate::phases`]). All off by default, which keeps S-14's behavior.
    #[serde(default)]
    pub phases: PhaseSettings,
}

impl Default for FlowSettings {
    fn default() -> Self {
        Self {
            max_teams: DEFAULT_MAX_TEAMS,
            seats_per_team: DEFAULT_SEATS_PER_TEAM,
            seat_slot_limit: None,
            friendly_unpause_ticks: DEFAULT_FRIENDLY_UNPAUSE_TICKS,
            start_delay_ticks: DEFAULT_START_DELAY_TICKS,
            pause_name_max: DEFAULT_PAUSE_NAME_MAX,
            max_join_asks: DEFAULT_MAX_JOIN_ASKS,
            max_pauses_per_seat: None,
            max_pause_ticks_per_seat: None,
            match_clock: MatchClock::Session,
            phases: PhaseSettings::default(),
        }
    }
}

/// One flow input, as it travels in a lockstep input stream or an agent call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "input", rename_all = "snake_case")]
pub enum FlowInput {
    /// Seat a player ([`MatchFlow::seat_player`]).
    Seat {
        /// Team.
        team: u8,
        /// Slot.
        slot: u8,
    },
    /// Ready for the next match, or ask to join when observing ([`MatchFlow::ready_up`]).
    Ready {
        /// Team.
        team: u8,
        /// Slot.
        slot: u8,
    },
    /// Take back a ready ([`MatchFlow::unready`]).
    Unready {
        /// Team.
        team: u8,
        /// Slot.
        slot: u8,
    },
    /// Watch instead of play ([`MatchFlow::observe`]).
    Observe {
        /// Team.
        team: u8,
        /// Slot.
        slot: u8,
    },
    /// Leave the match ([`MatchFlow::leave`]). The seat comes off the seated, ready and observer
    /// ledgers and off the join queue.
    Leave {
        /// Team.
        team: u8,
        /// Slot.
        slot: u8,
    },
    /// Approve the oldest join request of this team onto this seat ([`MatchFlow::approve_join`]).
    ApproveJoin {
        /// Team.
        team: u8,
        /// Slot.
        slot: u8,
    },
    /// Pause a friendly match under a name ([`MatchFlow::pause`]).
    Pause {
        /// The name on the banner.
        name: String,
    },
    /// Resume a friendly pause ([`MatchFlow::resume`]).
    ///
    /// `host` is deprecated and kept so older input bytes still decode. On the issuer path the
    /// flag is ignored: [`Issuer::host`] is the host, and the banner name grants nothing.
    Resume {
        /// Who asks. On the issuer path the banner name is display-only and does not grant a resume.
        name: String,
        /// Deprecated. Ignored on the issuer path.
        #[serde(default)]
        host: bool,
    },
    /// The host's pause switch ([`MatchFlow::host_set_paused`]).
    HostPause {
        /// Pause (`true`) or resume.
        on: bool,
    },
    /// Start without the seats that have not readied ([`MatchFlow::override_afk`]).
    OverrideAfk,
    /// End the match and wait for every seat to ready ([`MatchFlow::open_between`]).
    OpenBetween,
    /// Vote on this seat's team surrendering (`crate::victory`, GR-11.4). Own seat only. The flow
    /// checks the issuer and changes nothing; `crate::session::MatchSession::apply_from` records
    /// the vote.
    SurrenderVote {
        /// Team.
        team: u8,
        /// Slot.
        slot: u8,
        /// Yes (surrender) or no.
        yes: bool,
    },
}

/// What one [`MatchFlow::step`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowStep {
    /// The lockstep step that ran.
    pub step: u64,
    /// The next match started on this step.
    pub launched: bool,
    /// The match ran this step (the match tick advanced); `false` while paused.
    pub ran: bool,
    /// The match tick after the step.
    pub match_tick: u64,
}

/// Pause, ready, observer and join ledger of the running match.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MatchFlow {
    /// The settings.
    pub settings: FlowSettings,
    /// The match is paused (the match tick is frozen).
    pub paused: bool,
    /// The name on the pause banner; empty while running or under a host pause. Display only:
    /// resume authority is [`MatchFlow::paused_by_seat`] (or the host issuer), not this string.
    pub paused_by: String,
    /// The seat that paused a friendly match. `None` while running, under a host pause, or when
    /// the direct [`MatchFlow::pause`] call had no seat.
    #[serde(default)]
    pub paused_by_seat: Option<SeatId>,
    /// The match tick the pause started on.
    pub paused_at_tick: u64,
    /// Held steps under the current pause: the match tick is frozen, so the friendly-pause hold
    /// counts these. Same input stream, same count, on every peer.
    pub paused_for_ticks: u64,
    /// Between matches: the match stays paused until every seated, non-observing seat is ready, or
    /// the host overrides.
    pub between: bool,
    /// Seated players, in seat order (team, then slot): the ledgers are kept sorted, so one step's
    /// inputs give the same state in whatever order they arrived.
    pub seated: Vec<SeatId>,
    /// Seats that readied for the next match.
    pub ready: Vec<SeatId>,
    /// Seats that watch: they do not need to ready and see only their side.
    pub observers: Vec<SeatId>,
    /// Join requests, oldest first: sorted by the request step, then by seat.
    #[serde(default)]
    pub joins: Vec<JoinAsk>,
    /// Pauses and held ticks each seat has used, in seat order. Empty while budgets are off and
    /// nobody has paused through an issuer seat.
    #[serde(default)]
    pub pause_used: Vec<SeatPauseUse>,
    /// The agreed step the next match starts on, once every seat that must ready did.
    pub starts_at: Option<u64>,
    /// The match tick: advances on each step the match runs, frozen while paused.
    pub match_tick: u64,
    /// The lockstep step inputs land on now; [`MatchFlow::step`] runs it and moves on.
    pub step: u64,
    /// Events since the last [`MatchFlow::drain_events`].
    #[serde(skip)]
    pub events: Vec<RulesEvent>,
    /// Phase, round, phase clocks and round wins ([`crate::phases`]). Stays at its default while
    /// every phase setting is off.
    #[serde(default)]
    pub phases: PhaseState,
    /// Pending events kept before the oldest is dropped. Copied from
    /// [`crate::objective::ObjectiveSettings::max_pending_events`] when a session is created.
    /// `0` means [`DEFAULT_MAX_PENDING_EVENTS`]. Direct [`MatchFlow::new`] stores the default.
    #[serde(default = "default_max_pending_events")]
    pub max_pending_events: u32,
    /// Flow events dropped because the pending cap was full. Oldest first. `rules.match_get`
    /// serializes this counter. A host that drains every step leaves it at 0.
    #[serde(default)]
    pub events_dropped: u64,
}

fn default_max_pending_events() -> u32 {
    DEFAULT_MAX_PENDING_EVENTS
}

impl MatchFlow {
    /// A running match (not paused, not between) with `settings` and the default pending-event cap.
    pub fn new(settings: FlowSettings) -> Self {
        Self {
            settings,
            paused: false,
            paused_by: String::new(),
            paused_by_seat: None,
            paused_at_tick: 0,
            paused_for_ticks: 0,
            between: false,
            seated: Vec::new(),
            ready: Vec::new(),
            observers: Vec::new(),
            joins: Vec::new(),
            pause_used: Vec::new(),
            starts_at: None,
            match_tick: 0,
            step: 0,
            events: Vec::new(),
            phases: PhaseState::default(),
            max_pending_events: DEFAULT_MAX_PENDING_EVENTS,
            events_dropped: 0,
        }
    }
}

impl Default for MatchFlow {
    fn default() -> Self {
        Self::new(FlowSettings::default())
    }
}

impl MatchFlow {
    /// True when `seat` observes.
    pub fn is_observer(&self, seat: SeatId) -> bool {
        self.observers.contains(&seat)
    }

    /// `(ready, need)` among seated players who are not observing.
    pub fn ready_counts(&self) -> (u8, u8) {
        let need = self.seated.iter().filter(|s| !self.observers.contains(s)).count();
        let ready = self
            .seated
            .iter()
            .filter(|s| !self.observers.contains(s) && self.ready.contains(s))
            .count();
        (
            u8::try_from(ready).unwrap_or(u8::MAX),
            u8::try_from(need).unwrap_or(u8::MAX),
        )
    }

    /// Take the events since the last drain, oldest first.
    pub fn drain_events(&mut self) -> Vec<RulesEvent> {
        std::mem::take(&mut self.events)
    }

    /// Seated seats that do not observe, in seat order: the seats that play.
    pub fn playing_seats(&self) -> Vec<SeatId> {
        self.seated
            .iter()
            .copied()
            .filter(|s| !self.observers.contains(s))
            .collect()
    }

    /// Push one event, dropping the oldest when [`MatchFlow::max_pending_events`] is full.
    pub(crate) fn push_event(&mut self, event: RulesEvent) {
        push_capped(
            &mut self.events,
            &mut self.events_dropped,
            self.max_pending_events,
            event,
        );
    }

    /// Refuse a seat outside `1..=max_teams` or past `seats_per_team`.
    pub(crate) fn check_seat(&self, seat: SeatId) -> Result<(), RulesError> {
        if !(1..=self.settings.max_teams).contains(&seat.team) || seat.slot >= self.settings.seats_per_team {
            return Err(RulesError::SeatBad {
                team: seat.team,
                slot: seat.slot,
                max_teams: self.settings.max_teams,
                seats: self.settings.seats_per_team,
            });
        }
        Ok(())
    }

    /// Apply one input as the host ([`Issuer::host`]).
    ///
    /// Same as [`MatchFlow::apply_from`] with a host issuer, so existing callers keep working.
    /// `Resume.host` is ignored here too: the issuer is already the host.
    pub fn apply(&mut self, input: &FlowInput) -> Result<(), RulesError> {
        self.apply_from(Issuer::host(), input)
    }

    /// Apply one input from `issuer`. The issuer is not read from `input`.
    ///
    /// Own-seat inputs (`Ready`, `Unready`, `Observe`, `Leave`, `Pause`, `Resume`,
    /// `SurrenderVote`) need
    /// `issuer.seat` to be that seat, or `issuer.host`. `Seat` of one's own seat is allowed.
    /// `Seat` of another seat, `ApproveJoin`, `HostPause`, `OverrideAfk` and `OpenBetween` are
    /// host-only. A refusal is [`RulesError::NotPermitted`] and changes nothing. `Resume.host`
    /// is ignored; `issuer.host` is the host.
    pub fn apply_from(&mut self, issuer: Issuer, input: &FlowInput) -> Result<(), RulesError> {
        if !input_permitted(issuer, input) {
            return Err(RulesError::NotPermitted);
        }
        match input {
            FlowInput::Seat { team, slot } => self.seat_player(SeatId::new(*team, *slot)),
            FlowInput::Ready { team, slot } => self.ready_up(SeatId::new(*team, *slot)),
            FlowInput::Unready { team, slot } => self.unready(SeatId::new(*team, *slot)),
            FlowInput::Observe { team, slot } => self.observe(SeatId::new(*team, *slot)),
            FlowInput::Leave { team, slot } => self.leave(SeatId::new(*team, *slot)),
            FlowInput::ApproveJoin { team, slot } => self.approve_join(SeatId::new(*team, *slot)),
            FlowInput::Pause { name } => self.pause_as(issuer, name),
            FlowInput::Resume { .. } => self.resume_as(issuer),
            FlowInput::HostPause { on } => self.host_set_paused(*on),
            FlowInput::OverrideAfk => self.override_afk(),
            FlowInput::OpenBetween => {
                self.open_between();
                Ok(())
            }
            // The session records the vote; the flow only checks the issuer.
            FlowInput::SurrenderVote { .. } => Ok(()),
        }
    }

    /// Seat a player. A seat past the settings' slot limit is refused; a seat that is already
    /// seated stays as it is.
    pub fn seat_player(&mut self, seat: SeatId) -> Result<(), RulesError> {
        self.check_seat(seat)?;
        if let Some(limit) = self.settings.seat_slot_limit
            && seat.slot >= limit
        {
            return Err(RulesError::SeatLimit {
                slot: seat.slot,
                limit,
            });
        }
        insert_sorted(&mut self.seated, seat);
        self.reconsider_launch();
        Ok(())
    }

    /// Move `from` to `to` without a [`RulesEvent::SeatLeft`].
    ///
    /// The seat leaves `ready` and `observers`. A destination another player still holds is
    /// [`RulesError::SeatBad`]. The same seat is left as it is.
    pub fn relocate_seat(&mut self, from: SeatId, to: SeatId) -> Result<(), RulesError> {
        self.check_seat(to)?;
        if let Some(limit) = self.settings.seat_slot_limit
            && to.slot >= limit
        {
            return Err(RulesError::SeatLimit { slot: to.slot, limit });
        }
        if !self.seated.contains(&from) {
            return Err(RulesError::SeatMissing);
        }
        if from == to {
            return Ok(());
        }
        if self.seated.contains(&to) {
            return Err(RulesError::SeatBad {
                team: to.team,
                slot: to.slot,
                max_teams: self.settings.max_teams,
                seats: self.settings.seats_per_team,
            });
        }
        self.seated.retain(|seat| *seat != from);
        self.ready.retain(|seat| *seat != from);
        self.observers.retain(|seat| *seat != from);
        self.carry_seat_state(&[(from, to)]);
        insert_sorted(&mut self.seated, to);
        self.reconsider_launch();
        Ok(())
    }

    /// Move the per-player state that follows a seat move: the pauser seat, the pause budget
    /// ([`MatchFlow::pause_used`]) and join asks ([`MatchFlow::joins`], keeping their step).
    ///
    /// Every move applies at once, so a swap keeps both budgets. A budget row on a destination
    /// that is not itself moving belonged to someone else (a player who left) and is dropped, so
    /// the mover never inherits it, and the vacated seat starts the next occupant at zero. Moves
    /// whose source and destination are the same seat are ignored (ADR-8657).
    pub(crate) fn carry_seat_state(&mut self, moves: &[(SeatId, SeatId)]) {
        let moves: Vec<(SeatId, SeatId)> = moves.iter().copied().filter(|(from, to)| from != to).collect();
        if moves.is_empty() {
            return;
        }
        let dest_of = |seat: SeatId| moves.iter().find(|(from, _)| *from == seat).map(|(_, to)| *to);
        if let Some(seat) = self.paused_by_seat
            && let Some(to) = dest_of(seat)
        {
            self.paused_by_seat = Some(to);
        }
        let mut carried = Vec::new();
        self.pause_used.retain(|row| {
            if let Some(to) = dest_of(row.seat) {
                carried.push(SeatPauseUse { seat: to, ..*row });
                return false;
            }
            !moves.iter().any(|(_, to)| *to == row.seat)
        });
        for row in carried {
            match self.pause_used.binary_search_by_key(&row.seat, |kept| kept.seat) {
                Ok(index) => self.pause_used[index] = row,
                Err(index) => self.pause_used.insert(index, row),
            }
        }
        for ask in &mut self.joins {
            if let Some(to) = dest_of(ask.seat) {
                ask.seat = to;
            }
        }
        self.joins
            .sort_by(|a, b| a.step.cmp(&b.step).then(a.seat.cmp(&b.seat)));
        let mut seen: Vec<SeatId> = Vec::with_capacity(self.joins.len());
        self.joins.retain(|ask| {
            if seen.contains(&ask.seat) {
                return false;
            }
            seen.push(ask.seat);
            true
        });
    }

    /// The host's pause switch. Pausing works at any time during a live match. Between matches the
    /// ready gate starts the next one, so a resume there is refused.
    pub fn host_set_paused(&mut self, on: bool) -> Result<(), RulesError> {
        if self.between && !on {
            return Err(RulesError::ReadyToStart);
        }
        if on && !self.paused {
            self.paused_at_tick = self.match_tick;
            self.paused_for_ticks = 0;
            self.push_event(RulesEvent::Paused {
                tick: self.match_tick,
                by: self.paused_by.clone(),
            });
        }
        if !on && self.paused {
            self.push_event(RulesEvent::Resumed {
                tick: self.match_tick,
            });
        }
        self.paused = on;
        if !on {
            self.paused_by.clear();
            self.paused_by_seat = None;
            self.paused_for_ticks = 0;
        }
        Ok(())
    }

    /// Pause a friendly match. Anyone seated may; the banner names `name` (control characters
    /// dropped, cut to the settings' length, trimmed; empty reads as `host`). The match tick
    /// freezes, so the current match tick is the first held tick.
    pub fn pause(&mut self, name: &str) -> Result<(), RulesError> {
        if self.paused {
            return Err(RulesError::AlreadyPaused);
        }
        let trimmed: String = name
            .chars()
            .filter(|c| !c.is_control())
            .take(usize::from(self.settings.pause_name_max))
            .collect();
        let trimmed = trimmed.trim();
        self.paused = true;
        self.paused_by = if trimmed.is_empty() {
            "host".into()
        } else {
            trimmed.to_string()
        };
        self.paused_by_seat = None;
        self.paused_at_tick = self.match_tick;
        self.paused_for_ticks = 0;
        self.push_event(RulesEvent::Paused {
            tick: self.match_tick,
            by: self.paused_by.clone(),
        });
        Ok(())
    }

    /// True when `name` (or the host) may resume now. The pauser and the host may resume at once;
    /// anyone else waits out the settings' hold in held steps.
    pub fn may_resume(&self, name: &str, is_host: bool) -> bool {
        if !self.paused {
            return false;
        }
        if is_host || name.trim() == self.paused_by {
            return true;
        }
        self.paused_for_ticks >= self.settings.friendly_unpause_ticks
    }

    /// Resume a friendly match once [`MatchFlow::may_resume`] allows it.
    pub fn resume(&mut self, name: &str, is_host: bool) -> Result<(), RulesError> {
        if !self.paused {
            return Err(RulesError::NotPaused);
        }
        if self.between {
            return Err(RulesError::ReadyToStart);
        }
        if !self.may_resume(name, is_host) {
            return Err(RulesError::UnpauseHeld {
                by: self.paused_by.clone(),
                ticks_left: self
                    .settings
                    .friendly_unpause_ticks
                    .saturating_sub(self.paused_for_ticks),
            });
        }
        self.paused = false;
        self.paused_by.clear();
        self.paused_by_seat = None;
        self.paused_for_ticks = 0;
        self.push_event(RulesEvent::Resumed {
            tick: self.match_tick,
        });
        Ok(())
    }

    /// End this match and wait: every seated player has to ready, unless the host marks the
    /// missing ones AFK. Clears the readies and any scheduled start.
    pub fn open_between(&mut self) {
        self.between = true;
        self.paused = true;
        self.ready.clear();
        if self.starts_at.take().is_some() {
            self.push_event(RulesEvent::LaunchCancelled { step: self.step });
        }
    }

    /// Ready for the next match. An observer's ready is a join request and starts nothing; the host
    /// still has to approve it. When every seat that must ready did, the next match is scheduled
    /// on the agreed step [`MatchFlow::starts_at`].
    pub fn ready_up(&mut self, seat: SeatId) -> Result<(), RulesError> {
        self.check_seat(seat)?;
        if self.is_observer(seat) {
            self.enqueue_join(seat)?;
            return Ok(());
        }
        if !self.between {
            return Err(RulesError::MatchLive);
        }
        if !self.seated.contains(&seat) {
            return Err(RulesError::SeatMissing);
        }
        insert_sorted(&mut self.ready, seat);
        self.reconsider_launch();
        Ok(())
    }

    /// Take back a ready between matches; a scheduled start is called off.
    pub fn unready(&mut self, seat: SeatId) -> Result<(), RulesError> {
        self.check_seat(seat)?;
        if !self.between {
            return Err(RulesError::NotBetween);
        }
        self.ready.retain(|s| *s != seat);
        self.reconsider_launch();
        Ok(())
    }

    /// Watch instead of play: the seat no longer has to ready and sees only its side.
    pub fn observe(&mut self, seat: SeatId) -> Result<(), RulesError> {
        self.check_seat(seat)?;
        insert_sorted(&mut self.observers, seat);
        self.reconsider_launch();
        Ok(())
    }

    /// Start without the seats that have not readied: they become observers until they ready and
    /// the host approves them. The next match starts on the agreed step.
    pub fn override_afk(&mut self) -> Result<(), RulesError> {
        if !self.between {
            return Err(RulesError::NotBetween);
        }
        let unready: Vec<SeatId> = self
            .seated
            .iter()
            .copied()
            .filter(|s| !self.ready.contains(s) && !self.observers.contains(s))
            .collect();
        for seat in unready {
            insert_sorted(&mut self.observers, seat);
        }
        self.schedule_launch();
        Ok(())
    }

    /// The host approves the oldest join request of `seat`'s team onto `seat`: the asker and the
    /// seat leave the observers. The game moves the player onto the seat (and its AI off it).
    /// Other teams' asks stay in the queue. An empty queue is [`RulesError::NoJoinAsk`]. Asks
    /// that are all for other teams are [`RulesError::JoinTeam`], naming the oldest ask's team.
    pub fn approve_join(&mut self, seat: SeatId) -> Result<(), RulesError> {
        self.check_seat(seat)?;
        let Some(index) = self.joins.iter().position(|ask| ask.seat.team == seat.team) else {
            let Some(oldest) = self.joins.first() else {
                return Err(RulesError::NoJoinAsk);
            };
            return Err(RulesError::JoinTeam {
                asked: oldest.seat.team,
                team: seat.team,
            });
        };
        let ask = self.joins[index];
        self.observers.retain(|s| *s != ask.seat && *s != seat);
        self.joins.remove(index);
        Ok(())
    }

    /// Leave the match. The seat comes off `seated`, `ready`, `observers` and the join queue, and
    /// the ready gate is reconsidered. A pauser who leaves releases the friendly hold. A valid
    /// seat that was on none of those ledgers is a no-op.
    pub fn leave(&mut self, seat: SeatId) -> Result<(), RulesError> {
        self.check_seat(seat)?;
        let present = self.seated.contains(&seat)
            || self.ready.contains(&seat)
            || self.observers.contains(&seat)
            || self.joins.iter().any(|ask| ask.seat == seat);
        self.seated.retain(|s| *s != seat);
        self.ready.retain(|s| *s != seat);
        self.observers.retain(|s| *s != seat);
        self.joins.retain(|ask| ask.seat != seat);
        if self.paused_by_seat == Some(seat) {
            self.release_friendly_hold();
        }
        if present {
            self.push_event(RulesEvent::SeatLeft {
                step: self.step,
                team: seat.team,
                slot: seat.slot,
            });
        }
        self.reconsider_launch();
        Ok(())
    }

    /// What a viewer on `seat` sees of `other_team`: everything, unless the seat observes; an
    /// observer sees its own team, its allies (`allied(team, other)`) and neutrals (team 0).
    pub fn viewer_sees(&self, seat: SeatId, other_team: u8, allied: impl Fn(u8, u8) -> bool) -> bool {
        if !self.is_observer(seat) {
            return true;
        }
        other_team == 0 || other_team == seat.team || allied(seat.team, other_team)
    }

    /// Schedule the start when every seat that must ready did; call it off when one no longer has.
    fn reconsider_launch(&mut self) {
        if !self.between {
            return;
        }
        let mut need = self
            .seated
            .iter()
            .filter(|s| !self.observers.contains(s))
            .peekable();
        if need.peek().is_none() {
            return;
        }
        let all_ready = need.all(|s| self.ready.contains(s));
        if all_ready {
            self.schedule_launch();
        } else if self.starts_at.take().is_some() {
            self.push_event(RulesEvent::LaunchCancelled { step: self.step });
        }
    }

    fn schedule_launch(&mut self) {
        if self.starts_at.is_some() {
            return;
        }
        let at_step = self.step.saturating_add(self.settings.start_delay_ticks);
        self.starts_at = Some(at_step);
        self.push_event(RulesEvent::LaunchScheduled { at_step });
    }

    /// Run the current lockstep step: start the next match when its agreed step has come, count a
    /// held step while paused, or advance the match tick. Then move to the next step.
    pub fn step(&mut self) -> FlowStep {
        let now = self.step;
        self.step = self.step.saturating_add(1);
        let launched = self.between && self.starts_at.is_some_and(|at| at <= now);
        if launched {
            self.between = false;
            self.paused = false;
            self.paused_by.clear();
            self.paused_by_seat = None;
            self.paused_for_ticks = 0;
            self.ready.clear();
            self.starts_at = None;
            if self.settings.match_clock == MatchClock::PerMatch {
                self.match_tick = 0;
            }
        }
        if self.paused {
            self.paused_for_ticks = self.paused_for_ticks.saturating_add(1);
            self.charge_pause_tick();
            return FlowStep {
                step: now,
                launched,
                ran: false,
                match_tick: self.match_tick,
            };
        }
        self.match_tick = self.match_tick.saturating_add(1);
        FlowStep {
            step: now,
            launched,
            ran: true,
            match_tick: self.match_tick,
        }
    }

    fn banner_text(&self, name: &str) -> String {
        let trimmed: String = name
            .chars()
            .filter(|c| !c.is_control())
            .take(usize::from(self.settings.pause_name_max))
            .collect();
        trimmed.trim().to_string()
    }

    /// Pause from `issuer`. A blank name shows the seat label, never the word `host`. The seat
    /// is recorded in [`MatchFlow::paused_by_seat`]. A seat that has used its budget is refused
    /// and the match stays running.
    fn pause_as(&mut self, issuer: Issuer, name: &str) -> Result<(), RulesError> {
        if self.paused {
            return Err(RulesError::AlreadyPaused);
        }
        if let Some(seat) = issuer.seat
            && self.pause_blocked(seat)
        {
            return Err(RulesError::PauseBudget {
                team: seat.team,
                slot: seat.slot,
            });
        }
        let trimmed = self.banner_text(name);
        self.paused = true;
        self.paused_by = if trimmed.is_empty() {
            issuer.seat.map(seat_label).unwrap_or_default()
        } else {
            trimmed
        };
        self.paused_by_seat = issuer.seat;
        self.paused_at_tick = self.match_tick;
        self.paused_for_ticks = 0;
        if let Some(seat) = issuer.seat {
            self.bump_pauses(seat);
        }
        self.push_event(RulesEvent::Paused {
            tick: self.match_tick,
            by: self.paused_by.clone(),
        });
        Ok(())
    }

    fn may_resume_as(&self, issuer: Issuer) -> bool {
        if !self.paused {
            return false;
        }
        if issuer.host || (issuer.seat.is_some() && issuer.seat == self.paused_by_seat) {
            return true;
        }
        self.paused_for_ticks >= self.settings.friendly_unpause_ticks
    }

    /// Resume from `issuer`. The input's `host` flag is not consulted: only `issuer.host` is the
    /// host, and the banner name grants nothing.
    fn resume_as(&mut self, issuer: Issuer) -> Result<(), RulesError> {
        if !self.paused {
            return Err(RulesError::NotPaused);
        }
        if self.between {
            return Err(RulesError::ReadyToStart);
        }
        if !self.may_resume_as(issuer) {
            return Err(RulesError::UnpauseHeld {
                by: self.paused_by.clone(),
                ticks_left: self
                    .settings
                    .friendly_unpause_ticks
                    .saturating_sub(self.paused_for_ticks),
            });
        }
        self.paused = false;
        self.paused_by.clear();
        self.paused_by_seat = None;
        self.paused_for_ticks = 0;
        self.push_event(RulesEvent::Resumed {
            tick: self.match_tick,
        });
        Ok(())
    }

    /// Drop a friendly hold. Between matches the ready gate stays paused.
    fn release_friendly_hold(&mut self) {
        let friendly = self.paused_by_seat.is_some() || !self.paused_by.is_empty();
        self.paused_by_seat = None;
        self.paused_by.clear();
        self.paused_for_ticks = 0;
        if friendly && self.paused && !self.between {
            self.paused = false;
            self.push_event(RulesEvent::Resumed {
                tick: self.match_tick,
            });
        }
    }

    fn enqueue_join(&mut self, seat: SeatId) -> Result<(), RulesError> {
        if self.joins.iter().any(|ask| ask.seat == seat) {
            return Ok(());
        }
        if self.joins.len() >= usize::from(self.settings.max_join_asks) {
            return Err(RulesError::JoinQueueFull {
                cap: self.settings.max_join_asks,
            });
        }
        self.joins.push(JoinAsk {
            seat,
            step: self.step,
        });
        self.joins
            .sort_by(|a, b| a.step.cmp(&b.step).then(a.seat.cmp(&b.seat)));
        Ok(())
    }

    fn pause_blocked(&self, seat: SeatId) -> bool {
        let (pauses, ticks) = self
            .pause_used
            .iter()
            .find(|row| row.seat == seat)
            .map(|row| (row.pauses, row.ticks))
            .unwrap_or((0, 0));
        self.settings.max_pauses_per_seat.is_some_and(|max| pauses >= max)
            || self
                .settings
                .max_pause_ticks_per_seat
                .is_some_and(|max| ticks >= max)
    }

    fn bump_pauses(&mut self, seat: SeatId) {
        match self.pause_used.binary_search_by_key(&seat, |row| row.seat) {
            Ok(index) => {
                self.pause_used[index].pauses = self.pause_used[index].pauses.saturating_add(1);
            }
            Err(index) => self.pause_used.insert(
                index,
                SeatPauseUse {
                    seat,
                    pauses: 1,
                    ticks: 0,
                },
            ),
        }
    }

    fn bump_ticks(&mut self, seat: SeatId) -> u64 {
        match self.pause_used.binary_search_by_key(&seat, |row| row.seat) {
            Ok(index) => {
                self.pause_used[index].ticks = self.pause_used[index].ticks.saturating_add(1);
                self.pause_used[index].ticks
            }
            Err(index) => {
                self.pause_used.insert(
                    index,
                    SeatPauseUse {
                        seat,
                        pauses: 0,
                        ticks: 1,
                    },
                );
                1
            }
        }
    }

    fn charge_pause_tick(&mut self) {
        let Some(seat) = self.paused_by_seat else {
            return;
        };
        if self.settings.max_pause_ticks_per_seat.is_none() {
            return;
        }
        let used = self.bump_ticks(seat);
        if self
            .settings
            .max_pause_ticks_per_seat
            .is_some_and(|max| used >= max)
        {
            self.release_friendly_hold();
        }
    }
}

/// Own-seat inputs need the issuer's seat or the host. Seating another seat, approving a join,
/// the host pause, the AFK override and opening the ready gate are host-only.
fn input_permitted(issuer: Issuer, input: &FlowInput) -> bool {
    if issuer.host {
        return true;
    }
    match input {
        FlowInput::Ready { team, slot }
        | FlowInput::Unready { team, slot }
        | FlowInput::Observe { team, slot }
        | FlowInput::Leave { team, slot }
        | FlowInput::Seat { team, slot }
        | FlowInput::SurrenderVote { team, slot, .. } => issuer.seat == Some(SeatId::new(*team, *slot)),
        FlowInput::Pause { .. } | FlowInput::Resume { .. } => issuer.seat.is_some(),
        FlowInput::ApproveJoin { .. }
        | FlowInput::HostPause { .. }
        | FlowInput::OverrideAfk
        | FlowInput::OpenBetween => false,
    }
}

/// Insert `seat` into the sorted `list` unless it is there.
fn insert_sorted(list: &mut Vec<SeatId>, seat: SeatId) {
    if let Err(at) = list.binary_search(&seat) {
        list.insert(at, seat);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S10: SeatId = SeatId::new(1, 0);
    const S20: SeatId = SeatId::new(2, 0);
    const S21: SeatId = SeatId::new(2, 1);
    const S12: SeatId = SeatId::new(1, 2);

    /// A flow that starts the next match on the step the last ready lands on (the game's own
    /// behavior, used by the ported tests).
    fn immediate() -> MatchFlow {
        MatchFlow::new(FlowSettings {
            start_delay_ticks: 0,
            ..FlowSettings::default()
        })
    }

    fn no_allies(_: u8, _: u8) -> bool {
        false
    }

    #[test]
    fn host_pause_freezes_the_tick_and_releases_it() {
        let mut flow = immediate();
        let tick = flow.match_tick;
        flow.step();
        assert_eq!(flow.match_tick, tick + 1);
        flow.host_set_paused(true).expect("pause");
        flow.step();
        assert_eq!(flow.match_tick, tick + 1);
        flow.host_set_paused(false).expect("resume");
        flow.step();
        assert_eq!(flow.match_tick, tick + 2);
        assert_eq!(flow.step, 3, "the lockstep step always advances");
    }

    #[test]
    fn friendly_pause_names_the_pauser_and_gates_the_resume() {
        let mut flow = immediate();
        assert_eq!(DEFAULT_FRIENDLY_UNPAUSE_TICKS, 3600);
        let at = flow.match_tick;
        flow.pause("ada").expect("pause");
        assert!(flow.paused);
        assert_eq!(flow.paused_by, "ada");
        assert_eq!(flow.paused_at_tick, at);
        assert!(matches!(flow.drain_events().last(), Some(RulesEvent::Paused { by, .. }) if by == "ada"));
        // Pausing twice is refused.
        assert_eq!(flow.pause("bev"), Err(RulesError::AlreadyPaused));
        // The match tick freezes, but the hold still counts held steps.
        for _ in 0..DEFAULT_FRIENDLY_UNPAUSE_TICKS - 1 {
            flow.step();
        }
        assert_eq!(flow.match_tick, at, "the match tick stays frozen under a pause");
        assert!(!flow.may_resume("bev", false));
        assert_eq!(
            flow.resume("bev", false),
            Err(RulesError::UnpauseHeld {
                by: "ada".into(),
                ticks_left: 1
            })
        );
        // The pauser and the host may always resume.
        assert!(flow.may_resume("ada", false));
        assert!(flow.may_resume("bev", true));
        // One more held step opens it to anyone: 60 sim seconds, no wall clock.
        flow.step();
        assert!(flow.may_resume("bev", false));
        flow.resume("bev", false).expect("resume");
        assert!(!flow.paused);
        assert!(flow.paused_by.is_empty());
        assert!(matches!(
            flow.drain_events().last(),
            Some(RulesEvent::Resumed { .. })
        ));
        // Nothing to resume.
        assert_eq!(flow.resume("ada", false), Err(RulesError::NotPaused));
        // A blank name reads as the host; control characters drop; long names are cut.
        flow.pause("   ").expect("pause");
        assert_eq!(flow.paused_by, "host");
        flow.resume("", true).expect("resume");
        flow.pause("a\u{7}b-0123456789012345678901234567").expect("pause");
        assert_eq!(flow.paused_by, "ab-012345678901234567890");
    }

    #[test]
    fn ready_waits_for_every_seat_then_starts() {
        let mut flow = immediate();
        flow.seat_player(S10).expect("seat");
        flow.seat_player(S20).expect("seat");
        flow.open_between();
        assert!(flow.paused);
        let frozen = flow.match_tick;
        flow.step();
        assert_eq!(flow.match_tick, frozen);
        flow.ready_up(S10).expect("ready");
        assert!(flow.starts_at.is_none());
        assert!(!flow.step().launched);
        assert!(flow.paused);
        flow.ready_up(S20).expect("ready");
        let ran = flow.step();
        assert!(ran.launched && ran.ran);
        assert!(!flow.paused);
        assert!(!flow.between);
        assert!(flow.ready.is_empty());
        // Ready-up is closed while the match runs.
        assert_eq!(flow.ready_up(S10), Err(RulesError::MatchLive));
    }

    #[test]
    fn ready_up_refuses_bad_and_unseated_seats() {
        let mut flow = immediate();
        flow.open_between();
        assert!(matches!(
            flow.ready_up(SeatId::new(0, 0)),
            Err(RulesError::SeatBad { .. })
        ));
        assert!(matches!(
            flow.ready_up(SeatId::new(5, 0)),
            Err(RulesError::SeatBad { .. })
        ));
        assert!(matches!(
            flow.ready_up(SeatId::new(1, 16)),
            Err(RulesError::SeatBad { .. })
        ));
        assert_eq!(flow.ready_up(S10), Err(RulesError::SeatMissing));
        let mut capped = MatchFlow::new(FlowSettings {
            seat_slot_limit: Some(5),
            ..FlowSettings::default()
        });
        capped.seat_player(SeatId::new(1, 4)).expect("slot 4 seats");
        assert_eq!(
            capped.seat_player(SeatId::new(1, 5)),
            Err(RulesError::SeatLimit { slot: 5, limit: 5 })
        );
        assert!(
            !capped.seated.contains(&SeatId::new(1, 5)),
            "a seat past the limit is not taken"
        );
    }

    #[test]
    fn afk_override_starts_and_limits_that_player_to_their_team() {
        let mut flow = immediate();
        flow.seat_player(S10).expect("seat");
        flow.seat_player(S21).expect("seat");
        flow.open_between();
        flow.ready_up(S10).expect("ready");
        flow.override_afk().expect("override");
        assert!(flow.step().launched);
        assert!(!flow.paused);
        assert!(flow.is_observer(S21));
        assert!(flow.viewer_sees(S21, 2, no_allies));
        assert!(flow.viewer_sees(S21, 0, no_allies));
        assert!(!flow.viewer_sees(S21, 1, no_allies));
        assert!(
            flow.viewer_sees(S21, 1, |a, b| (a, b) == (2, 1)),
            "allies are visible"
        );
        assert!(flow.viewer_sees(S10, 2, no_allies));
        assert_eq!(flow.override_afk(), Err(RulesError::NotBetween));
    }

    #[test]
    fn host_approves_a_join_onto_a_seat_of_the_asking_team() {
        let mut flow = immediate();
        flow.observe(S12).expect("observe");
        flow.ready_up(S12).expect("an observer's ready is a join request");
        assert_eq!(flow.joins, vec![JoinAsk { seat: S12, step: 0 }]);
        assert_eq!(
            flow.approve_join(SeatId::new(2, 4)),
            Err(RulesError::JoinTeam { asked: 1, team: 2 })
        );
        assert!(flow.is_observer(S12));
        flow.approve_join(SeatId::new(1, 4)).expect("approve");
        assert!(!flow.is_observer(S12));
        assert!(flow.joins.is_empty());
        assert!(flow.viewer_sees(S12, 3, no_allies));
        assert_eq!(flow.approve_join(SeatId::new(1, 4)), Err(RulesError::NoJoinAsk));
    }

    #[test]
    fn ready_counts_leave_observers_out() {
        let mut flow = immediate();
        for seat in [S10, S20, S21] {
            flow.seat_player(seat).expect("seat");
        }
        flow.observe(S21).expect("observe");
        flow.open_between();
        assert_eq!(flow.ready_counts(), (0, 2));
        flow.ready_up(S20).expect("ready");
        assert_eq!(flow.ready_counts(), (1, 2));
    }

    #[test]
    fn ledgers_do_not_depend_on_input_order_within_a_step() {
        let mut a = immediate();
        let mut b = immediate();
        for seat in [S21, S10, S20] {
            a.seat_player(seat).expect("seat");
        }
        for seat in [S20, S21, S10] {
            b.seat_player(seat).expect("seat");
        }
        a.observe(S21).expect("observe");
        b.observe(S21).expect("observe");
        a.open_between();
        b.open_between();
        a.ready_up(S20).expect("ready");
        a.ready_up(S10).expect("ready");
        b.ready_up(S10).expect("ready");
        b.ready_up(S20).expect("ready");
        assert_eq!(a, b);
        assert_eq!(a.seated, [S10, S20, S21]);
    }

    #[test]
    fn between_matches_a_resume_is_refused() {
        let mut flow = immediate();
        flow.open_between();
        assert_eq!(flow.host_set_paused(false), Err(RulesError::ReadyToStart));
        assert_eq!(flow.resume("x", true), Err(RulesError::ReadyToStart));
        assert!(flow.paused);
    }

    #[test]
    fn an_unready_in_the_countdown_calls_the_start_off() {
        let mut flow = MatchFlow::new(FlowSettings {
            start_delay_ticks: 10,
            ..FlowSettings::default()
        });
        flow.seat_player(S10).expect("seat");
        flow.seat_player(S20).expect("seat");
        flow.open_between();
        flow.ready_up(S10).expect("ready");
        flow.ready_up(S20).expect("ready");
        assert_eq!(flow.starts_at, Some(10));
        for _ in 0..5 {
            assert!(!flow.step().launched);
        }
        flow.unready(S20).expect("unready");
        assert_eq!(flow.starts_at, None);
        assert!(
            flow.drain_events()
                .iter()
                .any(|e| matches!(e, RulesEvent::LaunchCancelled { step: 5 }))
        );
        for _ in 0..20 {
            assert!(!flow.step().launched, "no start without every ready");
        }
        flow.ready_up(S20).expect("ready again");
        assert_eq!(flow.starts_at, Some(35), "a new countdown from the new ready");
    }

    #[test]
    fn a_seat_cannot_ready_or_observe_another_seat() {
        let mut flow = immediate();
        flow.seat_player(S10).expect("seat");
        flow.seat_player(S20).expect("seat");
        flow.open_between();
        let guest = Issuer::guest(S10);
        for input in [
            FlowInput::Ready { team: 2, slot: 0 },
            FlowInput::Observe { team: 2, slot: 0 },
            FlowInput::Unready { team: 2, slot: 0 },
            FlowInput::Leave { team: 2, slot: 0 },
            FlowInput::Ready { team: 0, slot: 0 },
        ] {
            assert_eq!(flow.apply_from(guest, &input), Err(RulesError::NotPermitted));
        }
        assert!(flow.seated.contains(&S20));
        assert!(!flow.observers.contains(&S20));
        flow.apply_from(guest, &FlowInput::Ready { team: 1, slot: 0 })
            .expect("own ready");
        flow.apply_from(guest, &FlowInput::Observe { team: 1, slot: 0 })
            .expect("own observe");
        assert!(flow.ready.contains(&S10));
        assert!(flow.observers.contains(&S10));
    }

    #[test]
    fn a_guest_cannot_override_afk_approve_joins_or_host_pause() {
        let mut flow = immediate();
        flow.seat_player(S10).expect("seat");
        flow.seat_player(S20).expect("seat");
        flow.open_between();
        let before = flow.clone();
        let guest = Issuer::guest(S10);
        for input in [
            FlowInput::OverrideAfk,
            FlowInput::ApproveJoin { team: 1, slot: 0 },
            FlowInput::HostPause { on: true },
            FlowInput::OpenBetween,
            FlowInput::Seat { team: 2, slot: 0 },
        ] {
            assert_eq!(flow.apply_from(guest, &input), Err(RulesError::NotPermitted));
        }
        assert_eq!(flow.seated, before.seated);
        assert_eq!(flow.joins, before.joins);
        assert_eq!(flow.paused, before.paused);
        assert_eq!(flow.starts_at, before.starts_at);
        assert_eq!(flow, before);
    }

    #[test]
    fn a_guest_resume_with_the_host_flag_is_refused() {
        let mut flow = immediate();
        flow.apply_from(Issuer::guest(S10), &FlowInput::Pause { name: "ada".into() })
            .expect("pause");
        assert_eq!(
            flow.apply_from(
                Issuer::guest(S20),
                &FlowInput::Resume {
                    name: "ada".into(),
                    host: true,
                },
            ),
            Err(RulesError::UnpauseHeld {
                by: "ada".into(),
                ticks_left: DEFAULT_FRIENDLY_UNPAUSE_TICKS,
            })
        );
        assert!(flow.paused);
        flow.apply(&FlowInput::Resume {
            name: "nope".into(),
            host: false,
        })
        .expect("the host issuer resumes");
        assert!(!flow.paused);
    }

    #[test]
    fn an_empty_pause_name_grants_no_host_resume() {
        let mut flow = immediate();
        flow.apply_from(Issuer::guest(S10), &FlowInput::Pause { name: "  ".into() })
            .expect("pause");
        assert_eq!(flow.paused_by, seat_label(S10));
        assert_ne!(flow.paused_by, "host");
        assert_eq!(flow.paused_by_seat, Some(S10));
        let held = RulesError::UnpauseHeld {
            by: seat_label(S10),
            ticks_left: DEFAULT_FRIENDLY_UNPAUSE_TICKS,
        };
        assert_eq!(
            flow.apply_from(
                Issuer::guest(S20),
                &FlowInput::Resume {
                    name: "host".into(),
                    host: true,
                },
            ),
            Err(held.clone())
        );
        assert_eq!(
            flow.apply_from(
                Issuer::guest(S20),
                &FlowInput::Resume {
                    name: seat_label(S10),
                    host: true,
                },
            ),
            Err(held)
        );
        assert!(flow.paused);
        flow.apply_from(
            Issuer::guest(S10),
            &FlowInput::Resume {
                name: String::new(),
                host: true,
            },
        )
        .expect("the pauser resumes");
        assert!(!flow.paused);
    }

    #[test]
    fn a_seat_that_leaves_between_matches_no_longer_blocks_the_start() {
        let mut flow = immediate();
        flow.apply(&FlowInput::Seat { team: 1, slot: 0 }).expect("seat");
        flow.apply(&FlowInput::Seat { team: 2, slot: 0 }).expect("seat");
        flow.apply(&FlowInput::Seat { team: 2, slot: 1 }).expect("seat");
        flow.apply(&FlowInput::OpenBetween).expect("between");
        flow.apply_from(Issuer::guest(S21), &FlowInput::Leave { team: 2, slot: 1 })
            .expect("leave");
        assert!(
            flow.drain_events()
                .iter()
                .any(|event| matches!(event, RulesEvent::SeatLeft { team: 2, slot: 1, .. }))
        );
        assert!(!flow.seated.contains(&S21));
        assert!(!flow.ready.contains(&S21));
        assert!(!flow.observers.contains(&S21));
        assert!(flow.joins.iter().all(|ask| ask.seat != S21));
        flow.apply_from(Issuer::guest(S10), &FlowInput::Ready { team: 1, slot: 0 })
            .expect("ready");
        flow.apply_from(Issuer::guest(S20), &FlowInput::Ready { team: 2, slot: 0 })
            .expect("ready");
        assert!(flow.starts_at.is_some());
        assert!(
            flow.drain_events()
                .iter()
                .any(|event| matches!(event, RulesEvent::LaunchScheduled { .. }))
        );
    }

    #[test]
    fn two_join_asks_queue_in_order_and_the_cap_refuses_the_ninth() {
        let mut flow = immediate();
        for (team, slot) in [(1, 5), (1, 1)] {
            flow.apply(&FlowInput::Observe { team, slot }).expect("observe");
            flow.apply_from(
                Issuer::guest(SeatId::new(team, slot)),
                &FlowInput::Ready { team, slot },
            )
            .expect("ask");
        }
        assert_eq!(
            flow.joins,
            vec![
                JoinAsk {
                    seat: SeatId::new(1, 1),
                    step: 0
                },
                JoinAsk {
                    seat: SeatId::new(1, 5),
                    step: 0
                },
            ]
        );
        flow.apply_from(
            Issuer::guest(SeatId::new(1, 1)),
            &FlowInput::Ready { team: 1, slot: 1 },
        )
        .expect("a repeated ask is the same request");
        assert_eq!(flow.joins.len(), 2);
        flow.step();
        flow.apply(&FlowInput::Observe { team: 2, slot: 0 })
            .expect("observe");
        flow.apply_from(
            Issuer::guest(SeatId::new(2, 0)),
            &FlowInput::Ready { team: 2, slot: 0 },
        )
        .expect("ask");
        for slot in [2, 3, 4, 6, 7] {
            flow.apply(&FlowInput::Observe { team: 1, slot })
                .expect("observe");
            flow.apply_from(
                Issuer::guest(SeatId::new(1, slot)),
                &FlowInput::Ready { team: 1, slot },
            )
            .expect("ask");
        }
        assert_eq!(flow.joins.len(), 8);
        flow.apply(&FlowInput::Observe { team: 2, slot: 3 })
            .expect("observe");
        assert_eq!(
            flow.apply_from(
                Issuer::guest(SeatId::new(2, 3)),
                &FlowInput::Ready { team: 2, slot: 3 },
            ),
            Err(RulesError::JoinQueueFull { cap: 8 })
        );
        assert_eq!(flow.joins.len(), 8);
        flow.apply(&FlowInput::ApproveJoin { team: 1, slot: 0 })
            .expect("oldest team 1");
        assert!(flow.joins.iter().all(|ask| ask.seat != SeatId::new(1, 1)));
        assert_eq!(
            flow.joins
                .iter()
                .find(|ask| ask.seat.team == 1)
                .map(|ask| ask.seat),
            Some(SeatId::new(1, 5))
        );
        assert!(flow.joins.iter().any(|ask| ask.seat == SeatId::new(2, 0)));
    }

    #[test]
    fn pause_budget_refuses_the_extra_pause() {
        let guest = Issuer::guest(S10);
        let mut flow = MatchFlow::new(FlowSettings {
            max_pauses_per_seat: Some(1),
            ..FlowSettings::default()
        });
        flow.apply_from(guest, &FlowInput::Pause { name: "ada".into() })
            .expect("pause");
        flow.apply_from(
            guest,
            &FlowInput::Resume {
                name: "ada".into(),
                host: false,
            },
        )
        .expect("resume");
        assert_eq!(
            flow.apply_from(guest, &FlowInput::Pause { name: "ada".into() }),
            Err(RulesError::PauseBudget { team: 1, slot: 0 })
        );
        assert!(!flow.paused);

        let mut flow = MatchFlow::new(FlowSettings {
            max_pause_ticks_per_seat: Some(2),
            ..FlowSettings::default()
        });
        flow.apply_from(guest, &FlowInput::Pause { name: "ada".into() })
            .expect("pause");
        assert!(!flow.step().ran);
        assert!(flow.paused);
        assert!(!flow.step().ran);
        assert!(
            !flow.paused,
            "the held step that spends the budget releases the pause"
        );
        assert_eq!(
            flow.apply_from(guest, &FlowInput::Pause { name: "ada".into() }),
            Err(RulesError::PauseBudget { team: 1, slot: 0 })
        );
        assert!(!flow.paused);
    }

    #[test]
    fn flow_settings_defaults_match_the_switch_note() {
        let settings: FlowSettings = serde_json::from_str("{}").expect("decode");
        assert_eq!(settings.max_join_asks, DEFAULT_MAX_JOIN_ASKS);
        assert_eq!(settings.match_clock, MatchClock::Session);
        assert_eq!(settings.max_pauses_per_seat, None);
        assert_eq!(settings.max_pause_ticks_per_seat, None);
        let flow: MatchFlow =
            serde_json::from_str(r#"{"join":{"seat":{"team":1,"slot":2}}}"#).expect("old join");
        assert!(flow.joins.is_empty());
        assert_eq!(flow.paused_by_seat, None);
    }
}
