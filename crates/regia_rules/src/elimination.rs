// SPDX-License-Identifier: MPL-2.0
//! Team elimination, placements and the last side standing (GR-11.3, GR-11.4).
//!
//! A team that has played in the match is a contender. A contender is out when it has no playing
//! seat left that is not eliminated (every seat ran out of lives under its
//! [`crate::spawn::RespawnPolicy`], or left), when it surrendered, or when a victory condition
//! took its last tagged unit ([`crate::victory`]). Teams go out in elimination order; teams that
//! go out on the same tick go out in team-id order. A team's place is the count of contenders
//! still standing when it went out (itself included), so the first of 100 solo teams out is 100th.
//!
//! When at least one team is out and every team still standing is in one alliance, that side wins
//! ([`EndReason::LastStanding`] for a seat elimination; the cause of the last team out for the
//! others) and every contender of the alliance is placed first. When nobody is left standing (the
//! last sides went out on one tick) the match ends as [`EndReason::Draw`] with the tie order's
//! placements.

use serde::{Deserialize, Serialize};

use crate::match_flow::SeatId;
use crate::objective::{EndReason, RulesEvent};

/// Teams that win together (a strategy alliance). A team in no alliance stands alone.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Alliance {
    /// The allied teams.
    pub teams: Vec<u8>,
}

/// Elimination settings of a session (`SessionSettings::elimination`).
///
/// Off by default: `last_standing: false` with no victory condition keeps S-14's behavior (no
/// team is ever put out).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EliminationRules {
    /// A team whose seats are all eliminated is out, and the last side standing wins.
    pub last_standing: bool,
    /// Alliances: a side is an alliance (or a team in none). The first alliance that lists a team
    /// is its side.
    pub alliances: Vec<Alliance>,
}

impl EliminationRules {
    /// The side of `team`: the lowest team id of its alliance, or the team itself.
    pub fn side_of(&self, team: u8) -> u8 {
        self.alliances
            .iter()
            .find(|a| a.teams.contains(&team))
            .and_then(|a| a.teams.iter().copied().min())
            .unwrap_or(team)
    }

    /// True when `a` and `b` are on one side.
    pub fn allied(&self, a: u8, b: u8) -> bool {
        self.side_of(a) == self.side_of(b)
    }
}

/// Why a team went out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutCause {
    /// Every playing seat of the team is eliminated (or left).
    Eliminated,
    /// The team's surrender vote passed.
    Surrendered,
    /// The team has none of a `destroy_all_tagged` tag left.
    Destroyed,
    /// The host put the team out ([`crate::session::MatchSession::defeat_team`]).
    Host,
}

impl OutCause {
    /// The cause as a lowercase word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Eliminated => "eliminated",
            Self::Surrendered => "surrendered",
            Self::Destroyed => "destroyed",
            Self::Host => "host",
        }
    }

    /// The end reason when this cause leaves one side standing.
    pub fn end_reason(self) -> EndReason {
        match self {
            Self::Eliminated | Self::Host => EndReason::LastStanding,
            Self::Surrendered => EndReason::Surrendered,
            Self::Destroyed => EndReason::Destroyed,
        }
    }
}

/// One team out of the match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamOut {
    /// The team.
    pub team: u8,
    /// The match tick it went out on.
    pub tick: u64,
    /// Its place (1 is first).
    pub place: u8,
    /// Why.
    pub cause: OutCause,
}

/// One row of [`EliminationBook::placements`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    /// The team.
    pub team: u8,
    /// Its place (1 is first).
    pub place: u8,
}

/// Contenders, the elimination order and the winners of the running match. A launch (and a round
/// reset) clears it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EliminationBook {
    /// Teams that have played in this match, sorted.
    pub contenders: Vec<u8>,
    /// Teams out, in elimination order.
    pub out: Vec<TeamOut>,
    /// The winning side's contenders, sorted, once the match is decided by a side.
    pub winners: Vec<u8>,
    /// True once this book ended the match (or reported the end to the phase machine).
    pub decided: bool,
}

impl EliminationBook {
    /// Forget everything (a new match or round).
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// True when `team` is out.
    pub fn is_out(&self, team: u8) -> bool {
        self.out.iter().any(|o| o.team == team)
    }

    /// Contenders still standing, sorted.
    pub fn standing(&self) -> Vec<u8> {
        self.contenders
            .iter()
            .copied()
            .filter(|t| !self.is_out(*t))
            .collect()
    }

    /// Note every team in `seats` as a contender.
    pub fn note_contenders(&mut self, seats: &[SeatId]) {
        for seat in seats {
            if let Err(at) = self.contenders.binary_search(&seat.team) {
                self.contenders.insert(at, seat.team);
            }
        }
    }

    /// Put `teams` out on `tick` for `cause`, in team-id order, skipping teams that are out or are
    /// not contenders. Each gets the count of contenders still standing as its place, and one
    /// [`RulesEvent::TeamEliminated`].
    ///
    /// Teams this book already put out on the same tick for the same cause (the last rows of
    /// [`EliminationBook::out`], such as two host defeats between two steps) join this call's
    /// teams: the whole batch is placed again in team-id order, so the places do not depend on the
    /// call order (rules-genre-br-rts-review finding B). Their events still in `events` are
    /// replaced; a team whose event was already taken gets a second one only when its place moved.
    pub fn put_out(&mut self, teams: &[u8], tick: u64, cause: OutCause, events: &mut Vec<RulesEvent>) {
        let mut order: Vec<u8> = teams
            .iter()
            .copied()
            .filter(|team| !self.is_out(*team) && self.contenders.binary_search(team).is_ok())
            .collect();
        order.sort_unstable();
        order.dedup();
        if order.is_empty() {
            return;
        }
        let keep = self
            .out
            .iter()
            .rposition(|out| !(out.tick == tick && out.cause == cause))
            .map_or(0, |at| at + 1);
        let earlier = self.out.split_off(keep);
        let mut pending: Vec<u8> = Vec::new();
        events.retain(|event| match event {
            RulesEvent::TeamEliminated {
                tick: at,
                team,
                cause: why,
                ..
            } if *at == tick && *why == cause && earlier.iter().any(|out| out.team == *team) => {
                pending.push(*team);
                false
            }
            _ => true,
        });
        order.extend(earlier.iter().map(|out| out.team));
        order.sort_unstable();
        for team in order {
            let standing = self.standing().len();
            let place = u8::try_from(standing).unwrap_or(u8::MAX);
            self.out.push(TeamOut {
                team,
                tick,
                place,
                cause,
            });
            let before = earlier.iter().find(|out| out.team == team);
            if before.is_none_or(|out| out.place != place) || pending.contains(&team) {
                events.push(RulesEvent::TeamEliminated {
                    tick,
                    team,
                    place,
                    cause,
                });
            }
        }
    }

    /// When a team is out and every contender standing is on one side of `rules`, the decision:
    /// the winning team for `MatchOver` (the lowest standing id, `0` for a draw), its side's
    /// contenders and the end reason. `None` while two sides stand, or before anyone is out.
    pub fn decide(&self, rules: &EliminationRules) -> Option<(u8, Vec<u8>, EndReason)> {
        let last = self.out.last()?;
        let standing = self.standing();
        let Some(&first) = standing.first() else {
            return Some((0, Vec::new(), EndReason::Draw));
        };
        let side = rules.side_of(first);
        if standing.iter().any(|t| rules.side_of(*t) != side) {
            return None;
        }
        let winners: Vec<u8> = self
            .contenders
            .iter()
            .copied()
            .filter(|t| rules.side_of(*t) == side)
            .collect();
        Some((first, winners, last.cause.end_reason()))
    }

    /// Every placed team, best first, then by team id. The winners are first. Once decided, a
    /// team still standing that did not win (a hold or score victory) shares the place after the
    /// winners still standing. A team still standing in an undecided match has no place yet.
    pub fn placements(&self) -> Vec<Placement> {
        let mut rows: Vec<Placement> = self
            .winners
            .iter()
            .map(|&team| Placement { team, place: 1 })
            .collect();
        if self.decided {
            let standing = self.standing();
            let winners_up = standing.iter().filter(|t| self.winners.contains(t)).count();
            let place = u8::try_from(winners_up.saturating_add(1)).unwrap_or(u8::MAX);
            for team in standing {
                if !self.winners.contains(&team) {
                    rows.push(Placement { team, place });
                }
            }
        }
        for out in &self.out {
            if !self.winners.contains(&out.team) {
                rows.push(Placement {
                    team: out.team,
                    place: out.place,
                });
            }
        }
        rows.sort_by_key(|p| (p.place, p.team));
        rows
    }
}

/// The `last_standing` rule set (until the rule-set format lands): `teams` teams of `seats` seats,
/// one life each (`RespawnPolicy::Never`), last side standing, and `zone` when given.
pub fn last_standing_settings(
    teams: u8,
    seats: u8,
    zone: Option<crate::zone::ZoneSchedule>,
) -> crate::session::SessionSettings {
    use crate::match_flow::FlowSettings;
    use crate::objective::ObjectiveSettings;
    use crate::spawn::{RespawnPolicy, RespawnRules};
    crate::session::SessionSettings {
        flow: FlowSettings {
            max_teams: teams,
            seats_per_team: seats,
            ..FlowSettings::default()
        },
        objectives: ObjectiveSettings {
            max_teams: teams,
            ..ObjectiveSettings::default()
        },
        respawn: RespawnRules {
            default: RespawnPolicy::Never,
            ..RespawnRules::default()
        },
        zone,
        elimination: EliminationRules {
            last_standing: true,
            alliances: Vec::new(),
        },
        ..crate::session::SessionSettings::default()
    }
}

/// Teams in `contenders` (not out) with no playing seat left that is not `eliminated`.
pub fn teams_without_seats(book: &EliminationBook, playing: &[SeatId], eliminated: &[SeatId]) -> Vec<u8> {
    book.standing()
        .into_iter()
        .filter(|team| {
            !playing
                .iter()
                .any(|s| s.team == *team && eliminated.binary_search(s).is_err())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(teams: &[u8]) -> EliminationBook {
        let seats: Vec<SeatId> = teams.iter().map(|t| SeatId::new(*t, 0)).collect();
        let mut book = EliminationBook::default();
        book.note_contenders(&seats);
        book
    }

    /// ADR-8802: a second call on the same tick and cause re-places the batch in team-id order,
    /// replaces the first call's pending event, and re-announces a drained team only when its
    /// place moved. Another tick or another cause starts a new batch.
    #[test]
    fn same_tick_calls_merge_and_rewrite_their_events() {
        let mut book = book(&[1, 2, 3, 4]);
        let mut events = Vec::new();
        book.put_out(&[3], 5, OutCause::Host, &mut events);
        book.put_out(&[1], 5, OutCause::Host, &mut events);
        let rows: Vec<(u8, u8)> = book.out.iter().map(|o| (o.team, o.place)).collect();
        assert_eq!(rows, vec![(1, 4), (3, 3)]);
        let announced: Vec<(u8, u8)> = events
            .iter()
            .filter_map(|e| match e {
                RulesEvent::TeamEliminated { team, place, .. } => Some((*team, *place)),
                _ => None,
            })
            .collect();
        assert_eq!(
            announced,
            vec![(1, 4), (3, 3)],
            "one event per team, final places"
        );
        // The host drained the events; team 2 joins the batch. Team 1 keeps place 4 (no repeat),
        // team 2 takes 3 and team 3 moves to 2 (announced again).
        events.clear();
        book.put_out(&[2], 5, OutCause::Host, &mut events);
        let rows: Vec<(u8, u8)> = book.out.iter().map(|o| (o.team, o.place)).collect();
        assert_eq!(rows, vec![(1, 4), (2, 3), (3, 2)]);
        let announced: Vec<(u8, u8)> = events
            .iter()
            .filter_map(|e| match e {
                RulesEvent::TeamEliminated { team, place, .. } => Some((*team, *place)),
                _ => None,
            })
            .collect();
        assert_eq!(announced, vec![(2, 3), (3, 2)]);
        // The next tick does not reach back.
        events.clear();
        book.put_out(&[4], 6, OutCause::Host, &mut events);
        assert_eq!(book.out.last().map(|o| (o.team, o.place)), Some((4, 1)));
        assert_eq!(events.len(), 1);
        // Nothing to put out changes nothing.
        book.put_out(&[4, 9], 6, OutCause::Host, &mut events);
        assert_eq!(book.out.len(), 4);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn same_tick_outs_go_in_team_id_order_and_a_wipe_is_a_draw() {
        let rules = EliminationRules::default();
        let mut b = book(&[1, 2, 3, 4]);
        let mut events = Vec::new();
        b.put_out(&[3, 1], 5, OutCause::Eliminated, &mut events);
        assert_eq!(
            b.out.iter().map(|o| (o.team, o.place)).collect::<Vec<_>>(),
            vec![(1, 4), (3, 3)]
        );
        assert!(b.decide(&rules).is_none(), "two sides stand");
        b.put_out(&[4, 2], 6, OutCause::Eliminated, &mut events);
        assert_eq!(b.decide(&rules), Some((0, Vec::new(), EndReason::Draw)));
        assert_eq!(events.len(), 4);
    }

    #[test]
    fn a_team_out_twice_or_never_seated_is_skipped() {
        let mut b = book(&[1, 2]);
        let mut events = Vec::new();
        b.put_out(&[1, 1, 9], 1, OutCause::Host, &mut events);
        b.put_out(&[1], 2, OutCause::Host, &mut events);
        assert_eq!(b.out.len(), 1);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn the_last_standing_set_is_one_life_and_last_side() {
        let set = last_standing_settings(100, 1, None);
        assert_eq!(set.flow.max_teams, 100);
        assert_eq!(set.objectives.max_teams, 100);
        assert!(set.elimination.last_standing);
        assert_eq!(set.respawn.default, crate::spawn::RespawnPolicy::Never);
    }

    #[test]
    fn the_side_is_the_lowest_allied_id() {
        let rules = EliminationRules {
            last_standing: true,
            alliances: vec![Alliance { teams: vec![4, 2] }],
        };
        assert_eq!(rules.side_of(4), 2);
        assert_eq!(rules.side_of(3), 3);
        assert!(rules.allied(2, 4));
        assert!(!rules.allied(1, 4));
    }
}
