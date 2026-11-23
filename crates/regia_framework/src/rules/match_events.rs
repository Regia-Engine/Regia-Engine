// SPDX-License-Identifier: MPL-2.0
//! Match events for scripting (audit gap GR-7.6, `docs/specs/102-logic-graph.md` § 14): the
//! stable kind name of each [`MatchEvent`] and its fields flattened to plain values, so a logic
//! graph can trigger on `match.<kind>` and read who and what the event was about.
//!
//! The routing into logic-graph inboxes lives in the umbrella crate (`regia::logic_bridge`),
//! because this crate cannot depend on `regia_ai`. Everything here is plain data.

use regia_app::prelude::*;
use regia_rules::objective::{RulesEvent, ScoreReason};

use super::MatchEvent;

/// Marks a match-event listener: an entity whose logic graph hears every match event as
/// `match.<kind>` (`regia::logic_bridge` routes them). Put one on the level's game-mode entity
/// (the one that owns the gate, the announcer or the round logic). Several are allowed; each gets
/// every event. (Named `MatchEventListener` at the v0.7 roll-up, ADR-8731: `MatchStateEntity` is
/// the replicated match-state entity of `match_replication`.)
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Component, Default)]
pub struct MatchEventListener;

/// One [`MatchEvent`] flattened for scripting ([`MatchEvent::fields`]). A field the event does not
/// carry is `None`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MatchEventFields {
    /// The kind name ([`MatchEvent::kind`]).
    pub kind: &'static str,
    /// The match tick (or the flow step, for the flow's events: launches, seats leaving, phases).
    pub tick: u64,
    /// The team the event is about: the scoring, taking or winning team, a seat's team, the
    /// flag's own team for a drop or a return, the first winning team of a victory, or the team
    /// that is out.
    pub team: Option<u8>,
    /// The seat's slot on `team` (a scored, departed, respawning, eliminated or racing seat, or a
    /// surrender voter).
    pub seat: Option<u8>,
    /// The score after the event: the team's board score, or the seat's ledger score. Also the
    /// count of a wave spawn, the lap time in ticks of a racer lap, the place of a finished racer
    /// or of an eliminated team, and the damage of a zone damage tick.
    pub score: Option<i64>,
    /// The objective id (`ObjectiveEntity`): the flag of a flag event, the capture zone of a zone
    /// event, or the checkpoint of a racer checkpoint.
    pub objective: Option<u32>,
    /// The contestant id (an entity index) that took, dropped, returned or captured the flag, or
    /// that a shrinking zone damages.
    pub unit: Option<u32>,
    /// A reason word: the end reason, the score reason, the auto-return cause, the phase, the mode
    /// of a launch, who paused, the co-op end cause, the shrinking zone's stage, the out cause, a
    /// wave spawn's archetype, `best` for a best lap, `yes` / `no` for a surrender vote, or why the
    /// rotation passed over a row (`too_few_players`, `too_many_players`, `missing_content`,
    /// `recent`).
    pub reason: Option<String>,
    /// The round of a phase change or a round end; the wave index of a wave event, the lap of a
    /// racer event, the stage index of a shrinking-zone phase, or the playlist index of a skipped
    /// row.
    pub round: Option<u32>,
}

/// Every kind name [`MatchEvent::kind`] gives, in the core's variant order: the logic graph hears
/// each as `match.<kind>`, and the logic graph editor lists them under Events.
pub const MATCH_EVENT_KINDS: [&str; 38] = [
    "flag_taken",
    "flag_dropped",
    "flag_returned",
    "flag_captured",
    "hill_point",
    "match_over",
    "paused",
    "resumed",
    "launch_scheduled",
    "launch_cancelled",
    "launched",
    "seat_left",
    "phase_changed",
    "round_over",
    "flag_auto_returned",
    "player_scored",
    "respawn_at",
    "eliminated",
    "zone_captured",
    "zone_point",
    "zone_activated",
    "wave_started",
    "wave_spawn",
    "wave_cleared",
    "coop_ended",
    "racer_checkpoint",
    "racer_lap",
    "racer_missed_checkpoint",
    "racer_wrong_way",
    "racer_finished",
    "racer_dnf",
    "zone_phase",
    "zone_damage",
    "team_eliminated",
    "victory",
    "surrender_vote",
    "surrender_failed",
    "row_skipped",
];

/// The lowercase word of a [`ScoreReason`].
pub fn score_reason_str(reason: ScoreReason) -> &'static str {
    match reason {
        ScoreReason::Kill => "kill",
        ScoreReason::Death => "death",
        ScoreReason::Assist => "assist",
        ScoreReason::Objective => "objective",
        ScoreReason::Scripted => "scripted",
    }
}

impl MatchEvent {
    /// The prefix of every match event a logic graph hears: `match.<kind>`.
    pub const LOGIC_PREFIX: &'static str = "match.";

    /// The event's kind name, the same word `rules.match_step` reports as `kind`
    /// (`flag_captured`, `hill_point`, `match_over`, ...). A variant added to the core later than
    /// this build is `other`.
    pub fn kind(&self) -> &'static str {
        match &self.0 {
            RulesEvent::FlagTaken { .. } => "flag_taken",
            RulesEvent::FlagDropped { .. } => "flag_dropped",
            RulesEvent::FlagReturned { .. } => "flag_returned",
            RulesEvent::FlagCaptured { .. } => "flag_captured",
            RulesEvent::HillPoint { .. } => "hill_point",
            RulesEvent::MatchOver { .. } => "match_over",
            RulesEvent::Paused { .. } => "paused",
            RulesEvent::Resumed { .. } => "resumed",
            RulesEvent::LaunchScheduled { .. } => "launch_scheduled",
            RulesEvent::LaunchCancelled { .. } => "launch_cancelled",
            RulesEvent::Launched { .. } => "launched",
            RulesEvent::SeatLeft { .. } => "seat_left",
            RulesEvent::PhaseChanged { .. } => "phase_changed",
            RulesEvent::RoundOver { .. } => "round_over",
            RulesEvent::FlagAutoReturned { .. } => "flag_auto_returned",
            RulesEvent::PlayerScored { .. } => "player_scored",
            RulesEvent::RespawnAt { .. } => "respawn_at",
            RulesEvent::Eliminated { .. } => "eliminated",
            RulesEvent::ZoneCaptured { .. } => "zone_captured",
            RulesEvent::ZonePoint { .. } => "zone_point",
            RulesEvent::ZoneActivated { .. } => "zone_activated",
            RulesEvent::WaveStarted { .. } => "wave_started",
            RulesEvent::WaveSpawn { .. } => "wave_spawn",
            RulesEvent::WaveCleared { .. } => "wave_cleared",
            RulesEvent::CoopEnded { .. } => "coop_ended",
            RulesEvent::RacerCheckpoint { .. } => "racer_checkpoint",
            RulesEvent::RacerLap { .. } => "racer_lap",
            RulesEvent::RacerMissedCheckpoint { .. } => "racer_missed_checkpoint",
            RulesEvent::RacerWrongWay { .. } => "racer_wrong_way",
            RulesEvent::RacerFinished { .. } => "racer_finished",
            RulesEvent::RacerDnf { .. } => "racer_dnf",
            RulesEvent::ZonePhase { .. } => "zone_phase",
            RulesEvent::ZoneDamage { .. } => "zone_damage",
            RulesEvent::TeamEliminated { .. } => "team_eliminated",
            RulesEvent::Victory { .. } => "victory",
            RulesEvent::SurrenderVote { .. } => "surrender_vote",
            RulesEvent::SurrenderFailed { .. } => "surrender_failed",
            RulesEvent::RowSkipped { .. } => "row_skipped",
            _ => "other",
        }
    }

    /// The logic-graph event name: `match.<kind>` (`match.flag_captured`).
    pub fn logic_name(&self) -> String {
        format!("{}{}", Self::LOGIC_PREFIX, self.kind())
    }

    /// The event's fields as plain values (see [`MatchEventFields`]).
    pub fn fields(&self) -> MatchEventFields {
        let mut f = MatchEventFields {
            kind: self.kind(),
            ..MatchEventFields::default()
        };
        match &self.0 {
            RulesEvent::FlagTaken {
                tick,
                flag,
                unit,
                team,
                ..
            } => {
                f.tick = *tick;
                f.objective = Some(*flag);
                f.unit = Some(*unit);
                f.team = Some(*team);
            }
            RulesEvent::FlagDropped {
                tick,
                flag,
                unit,
                flag_team,
                ..
            }
            | RulesEvent::FlagReturned {
                tick,
                flag,
                unit,
                flag_team,
            } => {
                f.tick = *tick;
                f.objective = Some(*flag);
                f.unit = Some(*unit);
                f.team = Some(*flag_team);
            }
            RulesEvent::FlagCaptured {
                tick,
                flag,
                unit,
                team,
                score,
                ..
            } => {
                f.tick = *tick;
                f.objective = Some(*flag);
                f.unit = Some(*unit);
                f.team = Some(*team);
                f.score = Some(i64::from(*score));
            }
            RulesEvent::HillPoint { tick, team, score } => {
                f.tick = *tick;
                f.team = Some(*team);
                f.score = Some(i64::from(*score));
            }
            RulesEvent::MatchOver { tick, team, reason } => {
                f.tick = *tick;
                f.team = Some(*team);
                f.reason = Some(reason.as_str().to_string());
            }
            RulesEvent::Paused { tick, by } => {
                f.tick = *tick;
                f.reason = Some(by.clone());
            }
            RulesEvent::Resumed { tick } => f.tick = *tick,
            RulesEvent::LaunchScheduled { at_step } => f.tick = *at_step,
            RulesEvent::LaunchCancelled { step } => f.tick = *step,
            RulesEvent::Launched { step, mode, .. } => {
                f.tick = *step;
                f.reason = Some(mode.clone());
            }
            RulesEvent::SeatLeft { step, team, slot } => {
                f.tick = *step;
                f.team = Some(*team);
                f.seat = Some(*slot);
            }
            RulesEvent::PhaseChanged { step, phase, round } => {
                f.tick = *step;
                f.reason = Some(phase.as_str().to_string());
                f.round = Some(*round);
            }
            RulesEvent::RoundOver {
                tick,
                round,
                team,
                reason,
            } => {
                f.tick = *tick;
                f.team = Some(*team);
                f.round = Some(*round);
                f.reason = Some(reason.as_str().to_string());
            }
            RulesEvent::FlagAutoReturned {
                tick,
                flag,
                flag_team,
                cause,
            } => {
                f.tick = *tick;
                f.objective = Some(*flag);
                f.team = Some(*flag_team);
                f.reason = Some(cause.as_str().to_string());
            }
            RulesEvent::PlayerScored {
                tick,
                team,
                slot,
                reason,
                score,
            } => {
                f.tick = *tick;
                f.team = Some(*team);
                f.seat = Some(*slot);
                f.score = Some(*score);
                f.reason = Some(score_reason_str(*reason).to_string());
            }
            RulesEvent::RespawnAt { tick, team, slot }
            | RulesEvent::Eliminated { tick, team, slot }
            | RulesEvent::RacerWrongWay { tick, team, slot }
            | RulesEvent::RacerDnf { tick, team, slot } => {
                f.tick = *tick;
                f.team = Some(*team);
                f.seat = Some(*slot);
            }
            RulesEvent::ZoneCaptured { tick, zone, team, .. } => {
                f.tick = *tick;
                f.objective = Some(*zone);
                f.team = Some(*team);
            }
            RulesEvent::ZonePoint {
                tick,
                zone,
                team,
                score,
            } => {
                f.tick = *tick;
                f.objective = Some(*zone);
                f.team = Some(*team);
                f.score = Some(i64::from(*score));
            }
            RulesEvent::ZoneActivated { tick, zone } => {
                f.tick = *tick;
                f.objective = Some(*zone);
            }
            RulesEvent::WaveStarted { tick, wave } | RulesEvent::WaveCleared { tick, wave } => {
                f.tick = *tick;
                f.round = Some(*wave);
            }
            RulesEvent::WaveSpawn {
                tick,
                wave,
                archetype,
                count,
                ..
            } => {
                f.tick = *tick;
                f.round = Some(*wave);
                f.reason = Some(archetype.clone());
                f.score = Some(i64::from(*count));
            }
            RulesEvent::CoopEnded { tick, cause, .. } => {
                f.tick = *tick;
                f.reason = Some(cause.as_str().to_string());
            }
            RulesEvent::RacerCheckpoint {
                tick,
                team,
                slot,
                checkpoint,
                lap,
            } => {
                f.tick = *tick;
                f.team = Some(*team);
                f.seat = Some(*slot);
                f.objective = Some(*checkpoint);
                f.round = Some(*lap);
            }
            RulesEvent::RacerLap {
                tick,
                team,
                slot,
                lap,
                lap_ticks,
                best,
            } => {
                f.tick = *tick;
                f.team = Some(*team);
                f.seat = Some(*slot);
                f.round = Some(*lap);
                f.score = Some(i64::try_from(*lap_ticks).unwrap_or(i64::MAX));
                if *best {
                    f.reason = Some("best".to_string());
                }
            }
            RulesEvent::RacerMissedCheckpoint {
                tick,
                team,
                slot,
                checkpoint,
                ..
            } => {
                f.tick = *tick;
                f.team = Some(*team);
                f.seat = Some(*slot);
                f.objective = Some(*checkpoint);
            }
            RulesEvent::RacerFinished {
                tick,
                team,
                slot,
                place,
            } => {
                f.tick = *tick;
                f.team = Some(*team);
                f.seat = Some(*slot);
                f.score = Some(i64::from(*place));
            }
            RulesEvent::ZonePhase {
                tick, phase, stage, ..
            } => {
                f.tick = *tick;
                f.round = Some(*phase);
                f.reason = Some(stage.as_str().to_string());
            }
            RulesEvent::ZoneDamage { tick, unit, amount } => {
                f.tick = *tick;
                f.unit = Some(*unit);
                f.score = Some(i64::from(*amount));
            }
            RulesEvent::TeamEliminated {
                tick,
                team,
                place,
                cause,
            } => {
                f.tick = *tick;
                f.team = Some(*team);
                f.score = Some(i64::from(*place));
                f.reason = Some(cause.as_str().to_string());
            }
            RulesEvent::Victory { tick, teams, reason } => {
                f.tick = *tick;
                f.team = teams.first().copied();
                f.reason = Some(reason.as_str().to_string());
            }
            RulesEvent::SurrenderVote {
                tick,
                team,
                slot,
                yes,
            } => {
                f.tick = *tick;
                f.team = Some(*team);
                f.seat = Some(*slot);
                f.reason = Some(if *yes { "yes" } else { "no" }.to_string());
            }
            RulesEvent::SurrenderFailed { tick, team, .. } => {
                f.tick = *tick;
                f.team = Some(*team);
            }
            RulesEvent::RowSkipped {
                step, index, reason, ..
            } => {
                f.tick = *step;
                f.round = Some(*index);
                f.reason = Some(reason.code().to_string());
            }
            _ => {}
        }
        f
    }
}

/// One event of each core variant added after the S-14 sixteen (respawn, capture zones, co-op
/// waves, races, the shrinking zone, elimination and victory), for the kind and codec tests.
#[cfg(test)]
pub(crate) fn later_events() -> Vec<RulesEvent> {
    use regia_rules::elimination::OutCause;
    use regia_rules::objective::EndReason;
    use regia_rules::waves::CoopCause;
    use regia_rules::zone::ZoneStage;
    vec![
        RulesEvent::RespawnAt {
            tick: 30,
            team: 1,
            slot: 2,
        },
        RulesEvent::Eliminated {
            tick: 31,
            team: 2,
            slot: 0,
        },
        RulesEvent::ZoneCaptured {
            tick: 32,
            zone: 70,
            team: 1,
            previous: 2,
        },
        RulesEvent::ZonePoint {
            tick: 33,
            zone: 70,
            team: 1,
            score: 41,
        },
        RulesEvent::ZoneActivated { tick: 34, zone: 71 },
        RulesEvent::WaveStarted { tick: 35, wave: 3 },
        RulesEvent::WaveSpawn {
            tick: 36,
            wave: 3,
            group: 1,
            archetype: "grunt".to_owned(),
            count: 6,
            spawn_tag: "east".to_owned(),
        },
        RulesEvent::WaveCleared { tick: 37, wave: 3 },
        RulesEvent::CoopEnded {
            tick: 38,
            won: true,
            cause: CoopCause::AllWavesCleared,
        },
        RulesEvent::RacerCheckpoint {
            tick: 39,
            team: 1,
            slot: 0,
            checkpoint: 4,
            lap: 2,
        },
        RulesEvent::RacerLap {
            tick: 40,
            team: 1,
            slot: 0,
            lap: 2,
            lap_ticks: 3_600,
            best: true,
        },
        RulesEvent::RacerMissedCheckpoint {
            tick: 41,
            team: 1,
            slot: 1,
            checkpoint: 5,
            expected: 3,
        },
        RulesEvent::RacerWrongWay {
            tick: 42,
            team: 1,
            slot: 1,
        },
        RulesEvent::RacerFinished {
            tick: 43,
            team: 1,
            slot: 0,
            place: 1,
        },
        RulesEvent::RacerDnf {
            tick: 44,
            team: 2,
            slot: 1,
        },
        RulesEvent::ZonePhase {
            tick: 45,
            phase: 1,
            stage: ZoneStage::Shrinking,
            center: [10.5, -4.0],
            radius: 300.0,
            target_center: [12.0, -3.5],
            target_radius: 150.25,
        },
        RulesEvent::ZoneDamage {
            tick: 46,
            unit: 9,
            amount: 5,
        },
        RulesEvent::TeamEliminated {
            tick: 47,
            team: 3,
            place: 4,
            cause: OutCause::Surrendered,
        },
        RulesEvent::Victory {
            tick: 48,
            teams: vec![1, 4],
            reason: EndReason::LastStanding,
        },
        RulesEvent::SurrenderVote {
            tick: 49,
            team: 2,
            slot: 1,
            yes: false,
        },
        RulesEvent::SurrenderFailed {
            tick: 50,
            team: 2,
            until: 1_850,
        },
        // R-04 (4), ADR-8881: the rotation's skip reaches logic graphs and clients too.
        RulesEvent::RowSkipped {
            step: 51,
            index: 4,
            map_id: "quarry".to_owned(),
            reason: regia_rules::playlist::SkipReason::TooManyPlayers,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_rules::objective::EndReason;

    #[test]
    fn a_capture_reads_as_match_flag_captured_with_its_fields() {
        let event = MatchEvent(RulesEvent::FlagCaptured {
            tick: 12,
            flag: 2,
            unit: 7,
            team: 1,
            flag_team: 2,
            score: 3,
        });
        assert_eq!(event.logic_name(), "match.flag_captured");
        let f = event.fields();
        assert_eq!(
            (f.tick, f.team, f.objective, f.unit, f.score, f.seat),
            (12, Some(1), Some(2), Some(7), Some(3), None)
        );
    }

    /// ADR-8731: every core event has its own kind (none reads as `other`), the kind is the
    /// protocol's serde `kind` tag, and `MATCH_EVENT_KINDS` lists exactly the kinds the events give.
    #[test]
    fn every_core_event_has_a_listed_kind_equal_to_its_protocol_tag() {
        let mut events = vec![
            RulesEvent::FlagTaken {
                tick: 1,
                flag: 2,
                unit: 3,
                team: 1,
                flag_team: 2,
            },
            RulesEvent::FlagDropped {
                tick: 1,
                flag: 2,
                unit: 3,
                flag_team: 2,
                position: [0.0; 3],
            },
            RulesEvent::FlagReturned {
                tick: 1,
                flag: 2,
                unit: 3,
                flag_team: 2,
            },
            RulesEvent::FlagCaptured {
                tick: 1,
                flag: 2,
                unit: 3,
                team: 1,
                flag_team: 2,
                score: 1,
            },
            RulesEvent::HillPoint {
                tick: 1,
                team: 1,
                score: 1,
            },
            RulesEvent::MatchOver {
                tick: 1,
                team: 1,
                reason: EndReason::Ended,
            },
            RulesEvent::Paused {
                tick: 1,
                by: "host".to_owned(),
            },
            RulesEvent::Resumed { tick: 1 },
            RulesEvent::LaunchScheduled { at_step: 1 },
            RulesEvent::LaunchCancelled { step: 1 },
            RulesEvent::Launched {
                step: 1,
                index: None,
                map_id: String::new(),
                mode: String::new(),
            },
            RulesEvent::SeatLeft {
                step: 1,
                team: 1,
                slot: 0,
            },
            RulesEvent::PhaseChanged {
                step: 1,
                phase: regia_rules::phases::MatchPhase::Live,
                round: 0,
            },
            RulesEvent::RoundOver {
                tick: 1,
                round: 0,
                team: 1,
                reason: EndReason::Draw,
            },
            RulesEvent::FlagAutoReturned {
                tick: 1,
                flag: 2,
                flag_team: 2,
                cause: regia_rules::objective::AutoReturnCause::Timeout,
            },
            RulesEvent::PlayerScored {
                tick: 1,
                team: 1,
                slot: 0,
                reason: ScoreReason::Kill,
                score: 1,
            },
        ];
        events.extend(later_events());
        let mut seen = Vec::new();
        for event in events {
            let tag = serde_json::to_value(&event).expect("json")["kind"]
                .as_str()
                .map(str::to_string);
            let event = MatchEvent(event);
            assert_ne!(event.kind(), "other", "{:?}", event.0);
            assert_eq!(tag.as_deref(), Some(event.kind()));
            assert_eq!(event.fields().kind, event.kind());
            seen.push(event.kind());
        }
        seen.sort_unstable();
        seen.dedup();
        let mut listed = MATCH_EVENT_KINDS.to_vec();
        listed.sort_unstable();
        assert_eq!(seen, listed, "MATCH_EVENT_KINDS lists every kind once");
    }

    #[test]
    fn later_events_carry_their_fields() {
        let fields: Vec<MatchEventFields> = later_events()
            .into_iter()
            .map(|e| MatchEvent(e).fields())
            .collect();
        let by_kind = |kind: &str| fields.iter().find(|f| f.kind == kind).cloned().expect("kind");
        let zone = by_kind("zone_point");
        assert_eq!(
            (zone.objective, zone.team, zone.score),
            (Some(70), Some(1), Some(41))
        );
        let lap = by_kind("racer_lap");
        assert_eq!(
            (lap.team, lap.seat, lap.round, lap.score, lap.reason.as_deref()),
            (Some(1), Some(0), Some(2), Some(3_600), Some("best"))
        );
        let out = by_kind("team_eliminated");
        assert_eq!(
            (out.team, out.score, out.reason.as_deref()),
            (Some(3), Some(4), Some("surrendered"))
        );
        let won = by_kind("victory");
        assert_eq!(
            (won.team, won.reason.as_deref()),
            (Some(1), Some("last_standing"))
        );
        let spawn = by_kind("wave_spawn");
        assert_eq!(
            (spawn.round, spawn.score, spawn.reason.as_deref()),
            (Some(3), Some(6), Some("grunt"))
        );
        assert_eq!(by_kind("respawn_at").seat, Some(2));
        let skipped = by_kind("row_skipped");
        assert_eq!(
            (skipped.tick, skipped.round, skipped.reason.as_deref()),
            (51, Some(4), Some("too_many_players"))
        );
    }

    #[test]
    fn kind_names_match_the_protocol_kind_tag() {
        // `rules.match_step` reports events by their serde `kind` tag; the logic names agree.
        let events = [
            RulesEvent::MatchOver {
                tick: 1,
                team: 2,
                reason: EndReason::Ended,
            },
            RulesEvent::HillPoint {
                tick: 1,
                team: 1,
                score: 1,
            },
            RulesEvent::PlayerScored {
                tick: 1,
                team: 1,
                slot: 0,
                reason: ScoreReason::Scripted,
                score: 4,
            },
            RulesEvent::SeatLeft {
                step: 1,
                team: 1,
                slot: 0,
            },
        ];
        let mut unique = MATCH_EVENT_KINDS.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), MATCH_EVENT_KINDS.len(), "no kind listed twice");
        for event in events {
            assert!(MATCH_EVENT_KINDS.contains(&MatchEvent(event.clone()).kind()));
            let tag = serde_json::to_value(&event).expect("json")["kind"]
                .as_str()
                .map(str::to_string);
            let event = MatchEvent(event);
            assert_eq!(tag.as_deref(), Some(event.kind()));
        }
        let scored = MatchEvent(RulesEvent::PlayerScored {
            tick: 1,
            team: 1,
            slot: 0,
            reason: ScoreReason::Scripted,
            score: 4,
        });
        assert_eq!(scored.fields().reason.as_deref(), Some("scripted"));
    }
}
