// SPDX-License-Identifier: MPL-2.0
//! Bandwidth of the replicated match state (`book/agents/match-rules.md`, "Replicated match
//! state"): 64 players on 8 teams, 8 flags, 30 Hz snapshots of a 60 Hz match, for 60 seconds.
//! Prints JSON; asserts nothing:
//! `cargo run --release -p regia_framework --features net --example match_state_bandwidth`.
//!
//! The traffic: the clock runs every tick; a kill every second (two ledger rows change); a flag
//! is taken every 10 s, carried for 6 s and captured (a team score changes); one `MatchEventMsg`
//! per rules event. A carried flag's view keeps its base position (clients follow the carrier). Each snapshot sends a component only when its
//! wire bits changed since the last snapshot, as its `encode_delta` against the last sent value
//! (what replication sends). The replication layer's own framing (entity and component ids,
//! packet headers) is not counted: the numbers are the match state's payload.

use regia_core::math::Vec3;
use regia_framework::rules::match_replication::{
    MatchPhaseView, ObjectiveView, ObjectiveViewState, ObjectiveViews, PlayerScoreView, SeatView,
    TeamLabelView, TeamScoresView, encode_match_event,
};
use regia_framework::rules::objective::{RulesEvent, ScoreReason};
use regia_net::bits::BitWriter;
use regia_net::codec::{CodecContext, ReplicateCodec};

const PLAYERS: usize = 64;
const TEAMS: usize = 8;
const OBJECTIVES: usize = 8;
const TICK_HZ: u64 = 60;
const SNAPSHOT_EVERY: u64 = 2;
const SECONDS: u64 = 60;

/// Bits of `now` sent against `sent` (none when the wire bits are equal), and the new sent copy.
fn delta_bits<T: ReplicateCodec>(sent: &mut Option<T>, now: &T, cx: &CodecContext) -> usize {
    let bits = match sent {
        Some(prev) if prev.wire_eq(now, cx) => return 0,
        Some(prev) => {
            let mut w = BitWriter::new();
            now.encode_delta(prev, &mut w, cx);
            w.bit_len()
        }
        None => {
            let mut w = BitWriter::new();
            now.encode(&mut w, cx);
            w.bit_len()
        }
    };
    *sent = Some(now.clone());
    bits
}

#[derive(Default)]
struct Totals {
    phase: usize,
    teams: usize,
    objectives: usize,
    players: usize,
    events: usize,
    event_count: usize,
}

fn run(clock_resync_ticks: u64) -> Totals {
    let cx = CodecContext::detached();
    let mut phase = MatchPhaseView {
        phase: 2,
        time_left_ticks: Some(20 * 60 * TICK_HZ),
        ..MatchPhaseView::default()
    };
    let mut teams = TeamScoresView {
        scores: vec![0; TEAMS + 1],
        round_wins: Vec::new(),
        labels: (1..=TEAMS as u8)
            .map(|id| TeamLabelView {
                id,
                name: format!("team.{id}"),
                color: 0xff00_00ff,
            })
            .collect(),
    };
    let bases: Vec<Vec3> = (0..OBJECTIVES)
        .map(|i| Vec3::new(40.0 * (i as f32).cos(), 0.0, 40.0 * (i as f32).sin()))
        .collect();
    let mut objectives = ObjectiveViews {
        items: (0..OBJECTIVES)
            .map(|i| ObjectiveView {
                id: 900 + i as u32,
                kind: 0,
                team: i as u8 + 1,
                state: ObjectiveViewState::AtBase,
                position: bases[i],
                capture_progress: 0,
            })
            .collect(),
    };
    let mut players: Vec<PlayerScoreView> = (0..PLAYERS)
        .map(|i| PlayerScoreView {
            team: (i % TEAMS) as u8 + 1,
            slot: (i / TEAMS) as u8,
            row_id: (1 << 16) | (((i % TEAMS) as u64 + 1) << 8) | (i / TEAMS) as u64,
            ..PlayerScoreView::default()
        })
        .collect();
    let mut sent_phase = None;
    let mut sent_teams = None;
    let mut sent_objectives = None;
    let mut sent_players: Vec<Option<PlayerScoreView>> = vec![None; PLAYERS];
    let mut totals = Totals::default();
    let mut pending_events: Vec<RulesEvent> = Vec::new();
    for tick in 0..SECONDS * TICK_HZ {
        // The clock: rewritten every `clock_resync_ticks`.
        if tick.is_multiple_of(clock_resync_ticks) {
            phase.match_tick = tick;
            phase.time_left_ticks = Some((20 * 60 * TICK_HZ).saturating_sub(tick));
            phase.flow_step = tick;
            phase.net_tick = tick as u32;
        }
        // A kill every second.
        if tick % TICK_HZ == 30 {
            let killer = (tick / TICK_HZ * 7 % PLAYERS as u64) as usize;
            let victim = (killer + 9) % PLAYERS;
            players[killer].kills += 1;
            players[killer].score += 1;
            players[victim].deaths += 1;
            for (i, reason) in [(killer, ScoreReason::Kill), (victim, ScoreReason::Death)] {
                pending_events.push(RulesEvent::PlayerScored {
                    tick,
                    team: players[i].team,
                    slot: players[i].slot,
                    reason,
                    score: players[i].score,
                });
            }
        }
        // A raid every 10 s: take, carry 6 s, capture.
        let cycle = tick % (10 * TICK_HZ);
        let flag = (tick / (10 * TICK_HZ)) as usize % OBJECTIVES;
        let raider = (flag + 1) % PLAYERS;
        if cycle == 0 {
            objectives.items[flag].state = ObjectiveViewState::Carried {
                carrier: raider as u32,
                seat: Some(SeatView {
                    team: players[raider].team,
                    slot: players[raider].slot,
                }),
            };
            pending_events.push(RulesEvent::FlagTaken {
                tick,
                flag: 900 + flag as u32,
                unit: raider as u32,
                team: players[raider].team,
                flag_team: flag as u8 + 1,
            });
        } else if cycle == 6 * TICK_HZ {
            // While carried the view keeps the flag's base position (clients follow the carrier's
            // own replicated entity), so nothing is sent until the capture.
            let team = players[raider].team;
            teams.scores[usize::from(team)] += 1;
            players[raider].objective_points += 1;
            players[raider].score += 1;
            objectives.items[flag].state = ObjectiveViewState::AtBase;
            objectives.items[flag].position = bases[flag];
            pending_events.push(RulesEvent::FlagCaptured {
                tick,
                flag: 900 + flag as u32,
                unit: raider as u32,
                team,
                flag_team: flag as u8 + 1,
                score: teams.scores[usize::from(team)],
            });
        }
        // Events go out on the tick they happen (reliable channel, one message each).
        for event in pending_events.drain(..) {
            if let Some(bytes) = encode_match_event(&event) {
                // The message codec: a varint length, then the bytes.
                totals.events += 8 * (bytes.len() + 1);
                totals.event_count += 1;
            }
        }
        if !tick.is_multiple_of(SNAPSHOT_EVERY) {
            continue;
        }
        totals.phase += delta_bits(&mut sent_phase, &phase, &cx);
        totals.teams += delta_bits(&mut sent_teams, &teams, &cx);
        totals.objectives += delta_bits(&mut sent_objectives, &objectives, &cx);
        for (sent, now) in sent_players.iter_mut().zip(&players) {
            totals.players += delta_bits(sent, now, &cx);
        }
    }
    totals
}

fn main() {
    for resync in [1u64, 30] {
        let t = run(resync);
        let per_s = |bits: usize| bits as f64 / 8.0 / SECONDS as f64;
        let total = per_s(t.phase + t.teams + t.objectives + t.players + t.events);
        println!(
            "{{\"players\":{PLAYERS},\"teams\":{TEAMS},\"objectives\":{OBJECTIVES},\"tick_hz\":{TICK_HZ},\"snapshot_hz\":{},\"seconds\":{SECONDS},\"clock_resync_ticks\":{resync},\"bytes_per_s_per_client\":{{\"phase\":{:.1},\"team_scores\":{:.1},\"objectives\":{:.1},\"player_scores\":{:.1},\"events\":{:.1},\"total\":{:.1}}},\"events_sent\":{},\"framing\":\"not counted\"}}",
            TICK_HZ / SNAPSHOT_EVERY,
            per_s(t.phase),
            per_s(t.teams),
            per_s(t.objectives),
            per_s(t.players),
            per_s(t.events),
            total,
            t.event_count,
        );
    }
}
