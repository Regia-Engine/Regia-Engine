// SPDX-License-Identifier: MPL-2.0
//! Golden scenario `match_rules_ctf_koth_3600` (ADR-8503).
//!
//! One scripted session of 3600 lockstep steps: seats, a ready-up, a capture-the-flag raid with
//! deaths, a host pause, then a king-of-the-hill match. The harness hashes the session every 60
//! steps. The fingerprint walks the same fields a serialized session keeps (pending events are
//! skipped, matching `#[serde(skip)]` on the event logs).

use regia_detsim::golden::{DetRun, DetScenario, StateHasher};
use regia_detsim::tick::TickRate;

use crate::match_flow::{FlowInput, FlowSettings, MatchClock, MatchFlow, SeatId};
use crate::mode::MatchMode;
use crate::objective::{Contestant, EndReason, HillVolume, Objective, ObjectiveBoard, ObjectiveKind};
use crate::playlist::{Playlist, PlaylistEntry};
use crate::session::{CurrentRow, MatchSession, SessionSettings};

/// Golden file stem and harness name.
pub const SCENARIO_NAME: &str = "match_rules_ctf_koth_3600";

/// Seed recorded in the golden. The script does not draw from it; the harness still stores it.
pub const SCENARIO_SEED: u64 = 0x4D52_3600;

/// Lockstep steps in the session.
pub const SESSION_STEPS: u32 = 3600;

/// Session steps folded into one harness tick, so the golden keeps one hash per minute of sim.
pub const HASH_EVERY: u32 = 60;

const DELAY: u64 = 2;
const SEAT_STEP: u64 = 0;
const OPEN_STEP: u64 = 1;
const READY_STEP: u64 = 2;
const DEATH_A: u64 = 700;
const DEATH_B: u64 = 701;
const PAUSE_ON: u64 = 900;
const PAUSE_OFF: u64 = 960;
const SECOND_OPEN: u64 = 1800;
const SECOND_READY: u64 = 1801;
const SECOND_LAUNCH: u64 = SECOND_READY + DELAY;

/// The scripted capture-the-flag then king-of-the-hill session.
#[derive(Debug, Clone)]
pub struct MatchRulesCtfKoth3600 {
    session: MatchSession,
    /// Inputs the script failed to apply. Stays 0 for the committed run.
    apply_failures: u32,
    /// Objective placements the script failed. Stays 0 for the committed run.
    place_failures: u32,
}

impl MatchRulesCtfKoth3600 {
    /// The session at step 0, before any input.
    pub fn new() -> Self {
        Self::fresh()
    }

    fn fresh() -> Self {
        let mut session = MatchSession::new(SessionSettings {
            flow: FlowSettings {
                start_delay_ticks: DELAY,
                ..FlowSettings::default()
            },
            objectives: crate::objective::ObjectiveSettings {
                capture_limit: 100,
                hill_score_limit: 10_000,
                ..crate::objective::ObjectiveSettings::default()
            },
            ..SessionSettings::default()
        });
        session.playlist.set(vec![
            PlaylistEntry::new("harbor", "ctf"),
            PlaylistEntry::new("quarry", "koth"),
        ]);
        Self {
            session,
            apply_failures: 0,
            place_failures: 0,
        }
    }

    /// Apply this step's inputs, step the session, and place objectives when a row launches.
    pub fn play(&mut self, step: u64) {
        for input in inputs_for(step) {
            if self.session.apply(&input).is_err() {
                self.apply_failures = self.apply_failures.saturating_add(1);
            }
        }
        let units = units_for(step);
        let launched = self.session.step(&units).flow.launched;
        if launched {
            self.place_row();
        }
    }

    fn place_row(&mut self) {
        let mode = self
            .session
            .current
            .as_ref()
            .map(|row| row.entry.match_mode())
            .unwrap_or(self.session.board.mode);
        let placed = match mode {
            MatchMode::CaptureTheFlag => self.session.board.place_flags(&[1, 2], 8.0, 10).map(|_| ()),
            MatchMode::KingOfTheHill => self.session.board.install_hill_volume(
                20,
                HillVolume {
                    min: [-4.0, -1.0, -4.0],
                    max: [4.0, 3.0, 4.0],
                },
            ),
            _ => Ok(()),
        };
        if placed.is_err() {
            self.place_failures = self.place_failures.saturating_add(1);
        }
    }
}

impl Default for MatchRulesCtfKoth3600 {
    fn default() -> Self {
        Self::new()
    }
}

impl DetScenario for MatchRulesCtfKoth3600 {
    fn name(&self) -> &str {
        SCENARIO_NAME
    }

    fn default_run(&self) -> DetRun {
        DetRun::new(SCENARIO_SEED, SESSION_STEPS / HASH_EVERY, TickRate::DEFAULT)
    }

    fn reset(&mut self, _run: &DetRun) {
        *self = Self::fresh();
    }

    fn step(&mut self, tick: u32) {
        let _span = tracing::trace_span!("match_rules_ctf_koth_3600", tick).entered();
        let start = tick.saturating_mul(HASH_EVERY);
        for offset in 0..HASH_EVERY {
            self.play(u64::from(start.saturating_add(offset)));
        }
    }

    fn hash_state(&self, h: &mut StateHasher) {
        h.u32("apply_failures", self.apply_failures);
        h.u32("place_failures", self.place_failures);
        hash_session(h, &self.session);
    }
}

fn inputs_for(step: u64) -> Vec<FlowInput> {
    match step {
        SEAT_STEP => vec![
            FlowInput::Seat { team: 1, slot: 0 },
            FlowInput::Seat { team: 2, slot: 0 },
            FlowInput::Seat { team: 2, slot: 1 },
        ],
        OPEN_STEP | SECOND_OPEN => vec![FlowInput::OpenBetween],
        READY_STEP | SECOND_READY => vec![
            FlowInput::Ready { team: 1, slot: 0 },
            FlowInput::Ready { team: 2, slot: 0 },
            FlowInput::Ready { team: 2, slot: 1 },
        ],
        PAUSE_ON => vec![FlowInput::HostPause { on: true }],
        PAUSE_OFF => vec![FlowInput::HostPause { on: false }],
        _ => Vec::new(),
    }
}

fn units_for(step: u64) -> [Contestant; 2] {
    if step >= SECOND_LAUNCH {
        return [
            Contestant {
                id: 1001,
                team: 1,
                position: [0.0, 0.0, 0.0],
                alive: true,
            },
            Contestant {
                id: 2001,
                team: 2,
                position: [30.0, 0.0, 0.0],
                alive: true,
            },
        ];
    }
    let x = raid_x(step);
    let team1_alive = step != DEATH_A && step != DEATH_B;
    [
        Contestant {
            id: 1001,
            team: 1,
            position: [x, 0.0, 0.0],
            alive: team1_alive,
        },
        Contestant {
            id: 2001,
            team: 2,
            position: [-x, 0.0, 0.0],
            alive: true,
        },
    ]
}

/// Team 1's raid position on X: own base at phase 0, enemy base at phase 40, then back.
fn raid_x(step: u64) -> f32 {
    let phase = u16::try_from(step % 80).unwrap_or(0);
    let along = f32::from(phase);
    let along = if along < 40.0 { along } else { 80.0 - along };
    -8.0 + along * 0.4
}

fn hash_session(h: &mut StateHasher, session: &MatchSession) {
    hash_flow(h, &session.flow);
    hash_playlist(h, &session.playlist);
    hash_board(h, &session.board);
    match &session.current {
        None => h.bool("current", false),
        Some(row) => {
            h.bool("current", true);
            hash_row(h, row);
        }
    }
}

fn hash_flow(h: &mut StateHasher, flow: &MatchFlow) {
    h.item("flow", 0);
    hash_flow_settings(h, &flow.settings);
    h.bool("paused", flow.paused);
    hash_str(h, "paused_by", &flow.paused_by);
    match flow.paused_by_seat {
        Some(seat) => {
            h.bool("paused_by_seat", true);
            hash_seat(h, seat);
        }
        None => h.bool("paused_by_seat", false),
    }
    h.u64("paused_at_tick", flow.paused_at_tick);
    h.u64("paused_for_ticks", flow.paused_for_ticks);
    h.bool("between", flow.between);
    hash_seats(h, "seated", &flow.seated);
    hash_seats(h, "ready", &flow.ready);
    hash_seats(h, "observers", &flow.observers);
    h.u32("joins", u32_len(flow.joins.len()));
    for (i, ask) in flow.joins.iter().enumerate() {
        h.item("join", u32_index(i));
        hash_seat(h, ask.seat);
        h.u64("step", ask.step);
    }
    h.u32("pause_used", u32_len(flow.pause_used.len()));
    for (i, used) in flow.pause_used.iter().enumerate() {
        h.item("pause_used", u32_index(i));
        hash_seat(h, used.seat);
        h.u32("pauses", u32::from(used.pauses));
        h.u64("ticks", used.ticks);
    }
    match flow.starts_at {
        Some(at) => {
            h.bool("starts_at", true);
            h.u64("starts_at", at);
        }
        None => h.bool("starts_at", false),
    }
    h.u64("match_tick", flow.match_tick);
    h.u64("step", flow.step);
    h.u32("max_pending_events", flow.max_pending_events);
    h.u64("events_dropped", flow.events_dropped);
}

fn hash_flow_settings(h: &mut StateHasher, settings: &FlowSettings) {
    h.u32("max_teams", u32::from(settings.max_teams));
    h.u32("seats_per_team", u32::from(settings.seats_per_team));
    match settings.seat_slot_limit {
        Some(limit) => {
            h.bool("seat_slot_limit", true);
            h.u32("seat_slot_limit", u32::from(limit));
        }
        None => h.bool("seat_slot_limit", false),
    }
    h.u64("friendly_unpause_ticks", settings.friendly_unpause_ticks);
    h.u64("start_delay_ticks", settings.start_delay_ticks);
    h.u32("pause_name_max", u32::from(settings.pause_name_max));
    h.u32("max_join_asks", u32::from(settings.max_join_asks));
    match settings.max_pauses_per_seat {
        Some(limit) => {
            h.bool("max_pauses_per_seat", true);
            h.u32("max_pauses_per_seat", u32::from(limit));
        }
        None => h.bool("max_pauses_per_seat", false),
    }
    match settings.max_pause_ticks_per_seat {
        Some(limit) => {
            h.bool("max_pause_ticks_per_seat", true);
            h.u64("max_pause_ticks_per_seat", limit);
        }
        None => h.bool("max_pause_ticks_per_seat", false),
    }
    h.u32(
        "match_clock",
        match settings.match_clock {
            MatchClock::Session => 0,
            MatchClock::PerMatch => 1,
        },
    );
}

fn hash_playlist(h: &mut StateHasher, playlist: &Playlist) {
    h.item("playlist", 0);
    h.u32("index", playlist.index);
    h.bool("looping", playlist.looping);
    h.u32("entries", u32_len(playlist.entries.len()));
    for (i, entry) in playlist.entries.iter().enumerate() {
        h.item("entry", u32_index(i));
        hash_str(h, "map_id", &entry.map_id);
        hash_str(h, "mode", &entry.mode);
        h.u32("options", u32_len(entry.options.len()));
        for (j, (key, value)) in entry.options.iter().enumerate() {
            h.item("option", u32_index(j));
            hash_str(h, "key", key);
            hash_str(h, "value", value);
        }
    }
}

fn hash_board(h: &mut StateHasher, board: &ObjectiveBoard) {
    h.item("board", 0);
    h.u32("mode", mode_tag(board.mode));
    h.u32("max_teams", u32::from(board.settings.max_teams));
    h.u32("max_objectives", u32::from(board.settings.max_objectives));
    h.f32("flag_touch_radius", board.settings.flag_touch_radius);
    h.u32("capture_limit", board.settings.capture_limit);
    h.u32("hill_score_limit", board.settings.hill_score_limit);
    h.u32("hill_accrual_ticks", board.settings.hill_accrual_ticks);
    h.u32("max_pending_events", board.settings.max_pending_events);
    h.u32("objectives", u32_len(board.objectives.len()));
    for (i, objective) in board.objectives.iter().enumerate() {
        h.item("objective", u32_index(i));
        hash_objective(h, objective);
    }
    h.u32("team_scores", u32_len(board.team_scores.len()));
    for (i, score) in board.team_scores.iter().enumerate() {
        h.item("score", u32_index(i));
        h.u32("score", *score);
    }
    h.bool("match_over", board.match_over);
    match board.match_over_tick {
        Some(tick) => {
            h.bool("match_over_tick", true);
            h.u64("match_over_tick", tick);
        }
        None => h.bool("match_over_tick", false),
    }
    h.u32("end_reason", end_reason_tag(board.end_reason));
    h.u64("events_dropped", board.events_dropped);
    // Hashed only when something is locked, so the recorded goldens (no locks) keep their bits.
    // The set is hashed in its canonical order (sorted, unique), so a peer that kept a list in the
    // order a file stored it and a peer that sorted it hash the same lock set.
    if !board.locked.is_empty() {
        let mut locked = std::borrow::Cow::Borrowed(board.locked.as_slice());
        if !locked.is_sorted_by(|a, b| a < b) {
            let mut ids = locked.into_owned();
            crate::objective::canonical_locked(&mut ids);
            locked = std::borrow::Cow::Owned(ids);
        }
        h.u32("locked", u32_len(locked.len()));
        for (i, id) in locked.iter().enumerate() {
            h.item("locked", u32_index(i));
            h.u32("locked_id", *id);
        }
    }
}

fn hash_objective(h: &mut StateHasher, objective: &Objective) {
    h.u32("entity_id", objective.entity_id);
    h.u32(
        "kind",
        match objective.kind {
            ObjectiveKind::Flag => 0,
            ObjectiveKind::Hill => 1,
            // Capture zones (ADR-8649); the recorded match places none.
            ObjectiveKind::Zone => 2,
        },
    );
    h.u32("team_id", u32::from(objective.team_id));
    match objective.carrier_id {
        Some(id) => {
            h.bool("carrier_id", true);
            h.u32("carrier_id", id);
        }
        None => h.bool("carrier_id", false),
    }
    h.bool("at_base", objective.at_base);
    hash_pos(h, "base", objective.base_position);
    hash_pos(h, "pos", objective.position);
    match objective.volume {
        Some(volume) => {
            h.bool("volume", true);
            hash_pos(h, "min", volume.min);
            hash_pos(h, "max", volume.max);
        }
        None => h.bool("volume", false),
    }
}

fn hash_row(h: &mut StateHasher, row: &CurrentRow) {
    h.u32("index", row.index);
    hash_str(h, "map_id", &row.entry.map_id);
    hash_str(h, "mode", &row.entry.mode);
    h.u32("options", u32_len(row.entry.options.len()));
    for (i, (key, value)) in row.entry.options.iter().enumerate() {
        h.item("row_option", u32_index(i));
        hash_str(h, "key", key);
        hash_str(h, "value", value);
    }
}

fn hash_seats(h: &mut StateHasher, kind: &'static str, seats: &[SeatId]) {
    h.u32(kind, u32_len(seats.len()));
    for (i, seat) in seats.iter().enumerate() {
        h.item(kind, u32_index(i));
        hash_seat(h, *seat);
    }
}

fn hash_seat(h: &mut StateHasher, seat: SeatId) {
    h.u32("team", u32::from(seat.team));
    h.u32("slot", u32::from(seat.slot));
}

fn hash_pos(h: &mut StateHasher, name: &'static str, pos: [f32; 3]) {
    h.f32(name, pos[0]);
    h.f32(name, pos[1]);
    h.f32(name, pos[2]);
}

fn hash_str(h: &mut StateHasher, name: &'static str, text: &str) {
    let bytes = text.as_bytes();
    h.u32(name, u32_len(bytes.len()));
    let mut offset = 0;
    while offset < bytes.len() {
        let mut chunk = [0u8; 8];
        let end = (offset + 8).min(bytes.len());
        chunk[..end - offset].copy_from_slice(&bytes[offset..end]);
        h.u64(name, u64::from_le_bytes(chunk));
        offset = end;
    }
}

fn mode_tag(mode: MatchMode) -> u32 {
    match mode {
        MatchMode::TeamDeathmatch => 0,
        MatchMode::FreeForAll => 1,
        MatchMode::CaptureTheFlag => 2,
        MatchMode::KingOfTheHill => 3,
        MatchMode::Custom => 4,
        MatchMode::Domination => 5,
    }
}

fn end_reason_tag(reason: Option<EndReason>) -> u32 {
    match reason {
        None => 0,
        Some(EndReason::CaptureLimitReached) => 1,
        Some(EndReason::HillScoreLimitReached) => 2,
        Some(EndReason::Ended) => 3,
        // The phase machine's reasons (ADR-8485); the recorded match never ends by one.
        Some(EndReason::TimeLimit) => 4,
        Some(EndReason::Draw) => 5,
        Some(EndReason::Forfeit) => 6,
        Some(EndReason::RoundsWon) => 7,
        // The capture zones' reasons (ADR-8649); the recorded match never ends by one.
        Some(EndReason::ZoneScoreLimitReached) => 8,
        Some(EndReason::ZonesCaptured) => 9,
        // The genre reasons (GR-11.3, GR-11.4); the recorded match never ends by one. Numbered
        // after the capture zones' at the v0.7 roll-up (ADR-8731).
        Some(EndReason::LastStanding) => 10,
        Some(EndReason::Destroyed) => 11,
        Some(EndReason::Held) => 12,
        Some(EndReason::ScoreReached) => 13,
        Some(EndReason::Surrendered) => 14,
    }
}

fn u32_len(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

fn u32_index(index: usize) -> u32 {
    u32::try_from(index).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// rules-logic-nodes-review finding 1: a peer that kept a lock list in the order a file stored
    /// it and a peer that sorted it hold the same lock set, so they hash the same.
    #[test]
    fn the_lock_set_hashes_the_same_in_any_stored_order() {
        let hash_of = |locked: Vec<u32>| {
            let mut board = ObjectiveBoard::new(MatchMode::CaptureTheFlag, Default::default());
            board.locked = locked;
            let mut h = StateHasher::new();
            hash_board(&mut h, &board);
            h.finish()
        };
        let sorted = hash_of(vec![1, 2]);
        assert_eq!(hash_of(vec![2, 1]), sorted, "order of a stored list");
        assert_eq!(hash_of(vec![2, 1, 2, 1]), sorted, "repeats of a stored list");
        assert_ne!(
            hash_of(vec![1]),
            sorted,
            "a different lock set still hashes differently"
        );
    }

    #[test]
    fn the_3600_step_script_raids_pauses_and_holds_the_hill() {
        let mut scenario = MatchRulesCtfKoth3600::new();
        let mut carried_or_dropped = false;
        let mut paused = false;
        for step in 0..u64::from(SESSION_STEPS) {
            scenario.play(step);
            if step == PAUSE_ON {
                paused = scenario.session.flow.paused;
            }
            if step == PAUSE_OFF {
                assert!(
                    !scenario.session.flow.paused,
                    "the host pause ends at step {PAUSE_OFF}"
                );
            }
            if step < SECOND_LAUNCH
                && scenario
                    .session
                    .board
                    .objectives
                    .iter()
                    .any(|o| o.is_flag() && (o.carrier_id.is_some() || o.is_dropped()))
            {
                carried_or_dropped = true;
            }
        }
        assert_eq!(scenario.apply_failures, 0, "every scripted input applies");
        assert_eq!(scenario.place_failures, 0, "both rows place their objectives");
        assert!(paused, "the host pause holds at step {PAUSE_ON}");
        assert!(carried_or_dropped, "the raid takes or drops a flag");
        assert!(
            scenario.session.board.objectives.iter().any(|o| o.is_hill()),
            "the second row installs the hill"
        );
        assert_eq!(
            scenario
                .session
                .current
                .as_ref()
                .map(|row| row.entry.map_id.as_str()),
            Some("quarry")
        );
        assert_eq!(scenario.session.board.mode, MatchMode::KingOfTheHill);
        assert_eq!(scenario.session.board.events_dropped, 0);
        assert_eq!(scenario.session.flow.events_dropped, 0);
        assert_eq!(scenario.session.flow.step, u64::from(SESSION_STEPS));
    }
}
