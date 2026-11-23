// SPDX-License-Identifier: MPL-2.0
//! The one error type of the match rules core, with the stable lowercase codes the agent protocol
//! answers (`book/agents/match-rules.md`).

use thiserror::Error;

/// Why a match-flow input or an objective change was refused. The state is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RulesError {
    /// The seat's team or slot is outside the match's settings.
    #[error("seat team {team} slot {slot} is outside the match (teams 1..={max_teams}, {seats} slots)")]
    SeatBad {
        /// The team asked for.
        team: u8,
        /// The slot asked for.
        slot: u8,
        /// The highest fighting team.
        max_teams: u8,
        /// Slots per team.
        seats: u8,
    },
    /// The slot is past the seat limit of the match settings.
    #[error("slot {slot} is past the seat limit {limit}")]
    SeatLimit {
        /// The slot asked for.
        slot: u8,
        /// The first refused slot.
        limit: u8,
    },
    /// The seat is not seated.
    #[error("that seat is not seated")]
    SeatMissing,
    /// Ready-up is only open between matches; the match is live.
    #[error("the match is already running; ready up between matches")]
    MatchLive,
    /// The action needs the between-matches wait.
    #[error("not between matches")]
    NotBetween,
    /// No observer asked to join.
    #[error("no join request is waiting")]
    NoJoinAsk,
    /// The approved seat is on another team than the join request.
    #[error("the join request is for team {asked}, not team {team}")]
    JoinTeam {
        /// The team of the join request.
        asked: u8,
        /// The team of the seat offered.
        team: u8,
    },
    /// The match is already paused.
    #[error("the match is already paused")]
    AlreadyPaused,
    /// The match is not paused.
    #[error("the match is not paused")]
    NotPaused,
    /// Between matches the ready gate starts the next one; a resume does nothing.
    #[error("ready up to start the next match")]
    ReadyToStart,
    /// Only the pauser (or the host) may resume until the hold has passed.
    #[error("only {by} may resume for the next {ticks_left} ticks")]
    UnpauseHeld {
        /// Who paused.
        by: String,
        /// Held ticks left before anyone may resume.
        ticks_left: u64,
    },
    /// A flag's team is outside the fighting teams.
    #[error("flag team must be 1..={max_teams}, got {team}")]
    FlagTeam {
        /// The team asked for.
        team: u8,
        /// The highest fighting team.
        max_teams: u8,
    },
    /// The team already has a flag.
    #[error("team {team} already has a flag")]
    FlagExists {
        /// The team.
        team: u8,
    },
    /// A hill is already installed (one hill per match).
    #[error("hill already installed")]
    HillExists,
    /// The objective cap of the match settings is reached.
    #[error("objective cap reached ({cap})")]
    ObjectiveCap {
        /// The cap.
        cap: u16,
    },
    /// An objective's entity id is already used by another objective.
    #[error("objective entity {id} already exists")]
    ObjectiveId {
        /// The id.
        id: u32,
    },
    /// The issuer may not send this input. Host-only inputs and another seat's inputs are refused.
    /// The state is unchanged.
    #[error("that input is not permitted for this issuer")]
    NotPermitted,
    /// The join queue already holds `FlowSettings::max_join_asks` requests.
    #[error("the join queue is full ({cap})")]
    JoinQueueFull {
        /// The cap that was reached.
        cap: u8,
    },
    /// The seat has used its pause count or its held-tick budget.
    #[error("seat team {team} slot {slot} has used its pause budget")]
    PauseBudget {
        /// The team.
        team: u8,
        /// The slot.
        slot: u8,
    },
    /// A phase setting cannot run (a zero clock, zero rounds, fewer rounds than wins).
    #[error("phase setting `{field}` cannot run")]
    PhaseSettings {
        /// The setting.
        field: &'static str,
    },
    /// The vote is not open.
    #[error("the vote is closed")]
    VoteClosed,
    /// The ballot option is not on the vote.
    #[error("vote option {option} is not on the ballot")]
    VoteBadOption {
        /// The option index.
        option: usize,
    },
    /// The series length is not 1, 3, 5, 7 or 9.
    #[error("series length {length} is not 1, 3, 5, 7 or 9")]
    SeriesLength {
        /// The length asked for.
        length: u32,
    },
    /// Starting lives are outside the allowed range.
    #[error("lives {lives} is outside {min}..={max}")]
    LivesRange {
        /// The value asked for.
        lives: u32,
        /// The lowest allowed.
        min: u32,
        /// The highest allowed.
        max: u32,
    },
    /// A team change was refused by [`crate::teams::TeamSwitch`]. The seats are unchanged.
    #[error("team switch refused for team {team} slot {slot}")]
    TeamSwitchRefused {
        /// The seat that asked to switch.
        team: u8,
        /// The slot that asked to switch.
        slot: u8,
    },
    /// A named counter was refused: the row already holds the cap, or the name is empty or too long.
    #[error("named counter cap reached ({cap})")]
    StatCap {
        /// The cap that was reached, or the maximum name count when the name itself was refused.
        cap: usize,
    },
    /// A capture-zone setting cannot run (a zero rotation period, an empty rotation or order, the
    /// same attacking and defending team, a team outside the fighting teams).
    #[error("zone setting `{field}` cannot run")]
    ZoneSettings {
        /// The setting.
        field: &'static str,
    },
    /// A zone rule names an id that is not a capture zone on the board.
    #[error("objective {id} is not a capture zone")]
    ZoneUnknown {
        /// The id named.
        id: u32,
    },
    /// Co-op wave settings the director cannot play; `reason` names the check.
    #[error("wave settings refused: {reason}")]
    WaveSettings {
        /// The failed check (`empty_wave`, `zero_count`, `difficulty_not_ascending`, ...).
        reason: &'static str,
    },
    /// A race course or race settings the race cannot run; `reason` names the check.
    #[error("race settings refused: {reason}")]
    RaceSettings {
        /// The failed check (`too_few_checkpoints`, `next_unknown`, `laps_range`, ...).
        reason: &'static str,
    },
    /// A zone schedule a match cannot run (`crate::zone::ZoneSchedule::validate`).
    #[error("zone setting `{field}` is out of range")]
    ShrinkZoneSettings {
        /// The refused field.
        field: &'static str,
    },
    /// A victory setting or host report was refused: an empty or long tag, or a full report table.
    #[error("victory setting `{field}` is out of range")]
    VictorySettings {
        /// The refused field.
        field: &'static str,
    },
    /// A surrender vote while the match has no `surrender` victory condition, or is not playing.
    #[error("surrender is not open in this match")]
    SurrenderOff,
    /// A surrender vote while the team cools down after a failed ballot.
    #[error("team {team} may vote to surrender again on match tick {until}")]
    SurrenderCooldown {
        /// The team.
        team: u8,
        /// The first match tick it may vote again.
        until: u64,
    },
}

impl RulesError {
    /// The stable lowercase code of this error (agent protocol `data.code`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::SeatBad { .. } => "match_seat_bad",
            Self::SeatLimit { .. } => "match_seat_limit",
            Self::SeatMissing => "match_seat_missing",
            Self::MatchLive => "match_live",
            Self::NotBetween => "match_not_between",
            Self::NoJoinAsk => "match_no_join_ask",
            Self::JoinTeam { .. } => "match_join_team",
            Self::AlreadyPaused => "match_already_paused",
            Self::NotPaused => "match_not_paused",
            Self::ReadyToStart => "match_ready_to_start",
            Self::UnpauseHeld { .. } => "match_unpause_held",
            Self::FlagTeam { .. } => "match_flag_team",
            Self::FlagExists { .. } => "match_flag_exists",
            Self::HillExists => "match_hill_exists",
            Self::ObjectiveCap { .. } => "match_objective_cap",
            Self::ObjectiveId { .. } => "match_objective_id",
            Self::NotPermitted => "match_not_permitted",
            Self::JoinQueueFull { .. } => "match_join_queue_full",
            Self::PauseBudget { .. } => "match_pause_budget",
            Self::PhaseSettings { .. } => "match_phase_settings",
            Self::VoteClosed => "vote_closed",
            Self::VoteBadOption { .. } => "vote_bad_option",
            Self::SeriesLength { .. } => "series_length",
            Self::LivesRange { .. } => "lives_range",
            Self::TeamSwitchRefused { .. } => "match_team_switch_refused",
            Self::StatCap { .. } => "match_stat_cap",
            Self::ZoneSettings { .. } => "match_zone_settings",
            Self::ZoneUnknown { .. } => "match_zone_unknown",
            Self::WaveSettings { .. } => "match_wave_settings",
            Self::RaceSettings { .. } => "match_race_settings",
            Self::ShrinkZoneSettings { .. } => "match_shrink_zone_settings",
            Self::VictorySettings { .. } => "match_victory_settings",
            Self::SurrenderOff => "match_surrender_off",
            Self::SurrenderCooldown { .. } => "match_surrender_cooldown",
        }
    }

    /// Every code [`RulesError::code`] can return, for the book page and its coverage test.
    pub const CODES: [&str; 34] = [
        "match_seat_bad",
        "match_seat_limit",
        "match_seat_missing",
        "match_live",
        "match_not_between",
        "match_no_join_ask",
        "match_join_team",
        "match_already_paused",
        "match_not_paused",
        "match_ready_to_start",
        "match_unpause_held",
        "match_flag_team",
        "match_flag_exists",
        "match_hill_exists",
        "match_objective_cap",
        "match_objective_id",
        "match_not_permitted",
        "match_join_queue_full",
        "match_pause_budget",
        "match_phase_settings",
        "vote_closed",
        "vote_bad_option",
        "series_length",
        "lives_range",
        "match_team_switch_refused",
        "match_stat_cap",
        "match_zone_settings",
        "match_zone_unknown",
        "match_wave_settings",
        "match_race_settings",
        "match_shrink_zone_settings",
        "match_victory_settings",
        "match_surrender_off",
        "match_surrender_cooldown",
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_lowercase_unique_and_listed() {
        let samples = [
            RulesError::SeatBad {
                team: 0,
                slot: 0,
                max_teams: 4,
                seats: 16,
            },
            RulesError::SeatLimit { slot: 9, limit: 5 },
            RulesError::SeatMissing,
            RulesError::MatchLive,
            RulesError::NotBetween,
            RulesError::NoJoinAsk,
            RulesError::JoinTeam { asked: 1, team: 2 },
            RulesError::AlreadyPaused,
            RulesError::NotPaused,
            RulesError::ReadyToStart,
            RulesError::UnpauseHeld {
                by: "ada".into(),
                ticks_left: 1,
            },
            RulesError::FlagTeam {
                team: 5,
                max_teams: 4,
            },
            RulesError::FlagExists { team: 1 },
            RulesError::HillExists,
            RulesError::ObjectiveCap { cap: 8 },
            RulesError::ObjectiveId { id: 7 },
            RulesError::NotPermitted,
            RulesError::JoinQueueFull { cap: 8 },
            RulesError::PauseBudget { team: 1, slot: 0 },
            RulesError::PhaseSettings {
                field: "time_limit_ticks",
            },
            RulesError::VoteClosed,
            RulesError::VoteBadOption { option: 3 },
            RulesError::SeriesLength { length: 2 },
            RulesError::LivesRange {
                lives: 0,
                min: 1,
                max: 10,
            },
            RulesError::TeamSwitchRefused { team: 1, slot: 0 },
            RulesError::StatCap { cap: 16 },
            RulesError::ZoneSettings {
                field: "rotate_ticks",
            },
            RulesError::ZoneUnknown { id: 4 },
            RulesError::WaveSettings { reason: "empty_wave" },
            RulesError::RaceSettings {
                reason: "next_unknown",
            },
            RulesError::ShrinkZoneSettings { field: "radius" },
            RulesError::VictorySettings { field: "tag" },
            RulesError::SurrenderOff,
            RulesError::SurrenderCooldown { team: 1, until: 9 },
        ];
        let codes: Vec<&str> = samples.iter().map(RulesError::code).collect();
        assert_eq!(codes, RulesError::CODES);
        for code in codes {
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
        }
        let mut sorted = RulesError::CODES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), RulesError::CODES.len());
    }
}
