// SPDX-License-Identifier: MPL-2.0
//! Strategy victory conditions as data (GR-11.4): destroy every tagged unit, hold an objective
//! for a time, reach a score, eliminate the other sides, or vote to surrender.
//!
//! The host reports what only its simulation knows ([`VictoryState::report_tagged`],
//! [`VictoryState::report_holder`], [`VictoryState::report_score`]); the session evaluates the
//! conditions once per played step, after the objective rules:
//!
//! 1. Surrender ballots resolve: a ballot whose yes votes reach the team's threshold puts the team
//!    out; one that can no longer reach it, or whose window ran out, fails and starts the team's
//!    cooldown.
//! 2. A contender with none of a `destroy_all_tagged` tag left (it reported some before) is out.
//! 3. With `team_elimination` (or `last_standing`), a team whose seats are all eliminated is out.
//! 4. When a team is out and one side is left, that side wins (`crate::elimination`).
//! 5. `hold_for` and `score_threshold`, in list order: the holder's side, or the best score's
//!    side (ties: the lower team id), wins.
//!
//! Every side wins with its allies: [`RulesEvent::Victory`] lists the winning side's contenders
//! and `MatchOver` carries its lowest team id.

use serde::{Deserialize, Serialize};

use crate::RulesError;
use crate::match_flow::SeatId;
use crate::objective::RulesEvent;

/// Longest tag name, in bytes.
pub const MAX_TAG_LEN: usize = 32;

/// Most rows each host report table keeps (tagged counts, holders, scores).
pub const MAX_REPORT_ROWS: usize = 1024;

/// Default share of a team's voting seats a surrender needs, in percent.
pub const DEFAULT_SURRENDER_PERCENT: u8 = 100;

/// Default ticks a surrender ballot stays open (30 s at the default tick rate).
pub const DEFAULT_SURRENDER_WINDOW_TICKS: u64 = 30 * regia_detsim::tick::TICK_RATE_HZ as u64;

/// Default ticks after a failed ballot before the team may open another (3 min).
pub const DEFAULT_SURRENDER_COOLDOWN_TICKS: u64 = 180 * regia_detsim::tick::TICK_RATE_HZ as u64;

/// One way a strategy match can end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VictoryCondition {
    /// A team that had units with `tag` and has none left is out (a capital, a commander, every
    /// production building). The host reports the counts per team.
    DestroyAllTagged {
        /// The tag the host counts.
        tag: String,
    },
    /// A team that holds `objective` for `ticks` match ticks in a row wins (a wonder or a relic).
    /// A change of holder, or nobody holding, starts the count again.
    HoldFor {
        /// The objective id the host reports holders for.
        objective: u32,
        /// Match ticks to hold.
        ticks: u64,
    },
    /// The first team whose reported score reaches `score` wins (economy or points the game
    /// feeds). On one step the best score wins; a tie goes to the lower team id.
    ScoreThreshold {
        /// The score to reach.
        score: i64,
    },
    /// A team whose seats are all eliminated is out; the last side standing wins.
    TeamElimination,
    /// Teams may vote to surrender (`FlowInput::SurrenderVote`); a passed vote puts the team out.
    Surrender,
}

impl VictoryCondition {
    /// The condition as a lowercase word.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DestroyAllTagged { .. } => "destroy_all_tagged",
            Self::HoldFor { .. } => "hold_for",
            Self::ScoreThreshold { .. } => "score_threshold",
            Self::TeamElimination => "team_elimination",
            Self::Surrender => "surrender",
        }
    }
}

/// Surrender vote settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SurrenderRules {
    /// Share of the team's voting seats (playing, not eliminated) that must vote yes, in percent
    /// (`1..=100`; `0` reads as 1). The count rounds up and is at least one seat.
    pub threshold_percent: u8,
    /// Match ticks a ballot stays open after its first vote.
    pub window_ticks: u64,
    /// Match ticks after a failed ballot before the team may vote again.
    pub cooldown_ticks: u64,
}

impl Default for SurrenderRules {
    fn default() -> Self {
        Self {
            threshold_percent: DEFAULT_SURRENDER_PERCENT,
            window_ticks: DEFAULT_SURRENDER_WINDOW_TICKS,
            cooldown_ticks: DEFAULT_SURRENDER_COOLDOWN_TICKS,
        }
    }
}

impl SurrenderRules {
    /// Yes votes a team of `voters` seats needs (rounded up, at least 1).
    pub fn needed(&self, voters: usize) -> usize {
        let percent = usize::from(self.threshold_percent.clamp(1, 100));
        voters.saturating_mul(percent).div_ceil(100).max(1)
    }
}

/// Victory settings of a session (`SessionSettings::victory`). Empty by default: no condition is
/// checked (S-14's behavior).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VictoryRules {
    /// Conditions, checked in the order of the module docs; `hold_for` and `score_threshold` in
    /// list order.
    pub conditions: Vec<VictoryCondition>,
    /// The surrender vote, used with [`VictoryCondition::Surrender`].
    pub surrender: SurrenderRules,
}

impl VictoryRules {
    /// True when any condition is set.
    pub fn is_on(&self) -> bool {
        !self.conditions.is_empty()
    }

    /// True when `condition`'s kind is listed.
    pub fn has(&self, kind: &str) -> bool {
        self.conditions.iter().any(|c| c.as_str() == kind)
    }

    /// Refuse a tag that is empty or longer than [`MAX_TAG_LEN`] bytes.
    pub fn validate(&self) -> Result<(), RulesError> {
        for condition in &self.conditions {
            if let VictoryCondition::DestroyAllTagged { tag } = condition
                && (tag.is_empty() || tag.len() > MAX_TAG_LEN)
            {
                return Err(RulesError::VictorySettings { field: "tag" });
            }
        }
        Ok(())
    }
}

/// How many units with one tag a team has, as the host last reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaggedCount {
    /// The team.
    pub team: u8,
    /// The tag.
    pub tag: String,
    /// Units left.
    pub count: u32,
    /// The team has reported at least one such unit this match.
    pub seen: bool,
}

/// Who holds one objective and since when.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoldRow {
    /// The objective id.
    pub objective: u32,
    /// The holding team; `0` for nobody.
    pub holder: u8,
    /// The match tick the holder took it.
    pub since: u64,
}

/// A team's reported score.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoreRow {
    /// The team.
    pub team: u8,
    /// Its score.
    pub score: i64,
}

/// An open surrender ballot of one team.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurrenderBallot {
    /// The team.
    pub team: u8,
    /// The match tick the first vote landed on.
    pub opened: u64,
    /// Slots that voted yes, sorted.
    pub yes: Vec<u8>,
    /// Slots that voted no, sorted.
    pub no: Vec<u8>,
}

/// A team that may not open a ballot before `until`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurrenderCooldown {
    /// The team.
    pub team: u8,
    /// The first match tick it may vote again.
    pub until: u64,
}

/// What the host reported and the open ballots of the running match, all in sorted vectors. A
/// launch clears it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VictoryState {
    /// Tagged counts, sorted by team then tag.
    pub tagged: Vec<TaggedCount>,
    /// Objective holders, sorted by objective.
    pub holds: Vec<HoldRow>,
    /// Scores, sorted by team.
    pub scores: Vec<ScoreRow>,
    /// Open ballots, sorted by team.
    pub ballots: Vec<SurrenderBallot>,
    /// Cooldowns, sorted by team.
    pub cooldowns: Vec<SurrenderCooldown>,
}

impl VictoryState {
    /// Forget everything (a new match).
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Record that `team` has `count` units with `tag`. A tag that is empty or too long, or a
    /// new row past [`MAX_REPORT_ROWS`], is refused ([`RulesError::VictorySettings`]).
    pub fn report_tagged(&mut self, team: u8, tag: &str, count: u32) -> Result<(), RulesError> {
        if tag.is_empty() || tag.len() > MAX_TAG_LEN {
            return Err(RulesError::VictorySettings { field: "tag" });
        }
        match self
            .tagged
            .binary_search_by(|row| (row.team, row.tag.as_str()).cmp(&(team, tag)))
        {
            Ok(at) => {
                let row = &mut self.tagged[at];
                row.count = count;
                row.seen |= count > 0;
            }
            Err(at) => {
                check_rows(self.tagged.len())?;
                self.tagged.insert(
                    at,
                    TaggedCount {
                        team,
                        tag: tag.to_string(),
                        count,
                        seen: count > 0,
                    },
                );
            }
        }
        Ok(())
    }

    /// Check a batch of reports before any of it lands (rules-genre-br-rts-review finding C):
    /// `tagged` as `(team, tag)`, `holders` as objective ids, `scores` as teams. Refused
    /// ([`RulesError::VictorySettings`]) when a tag is empty or too long (`tag`), or when the
    /// batch's new keys, each counted once, would take a table past [`MAX_REPORT_ROWS`] (`rows`).
    /// Updates of keys already held add nothing. `Ok` means the same batch applied with
    /// [`VictoryState::report_tagged`], [`VictoryState::report_holder`] and
    /// [`VictoryState::report_score`] cannot fail.
    pub fn check_report(
        &self,
        tagged: &[(u8, &str)],
        holders: &[u32],
        scores: &[u8],
    ) -> Result<(), RulesError> {
        if tagged
            .iter()
            .any(|(_, tag)| tag.is_empty() || tag.len() > MAX_TAG_LEN)
        {
            return Err(RulesError::VictorySettings { field: "tag" });
        }
        let mut new_tags: Vec<(u8, &str)> = tagged
            .iter()
            .copied()
            .filter(|(team, tag)| {
                self.tagged
                    .binary_search_by(|row| (row.team, row.tag.as_str()).cmp(&(*team, *tag)))
                    .is_err()
            })
            .collect();
        new_tags.sort_unstable();
        new_tags.dedup();
        let mut new_holds: Vec<u32> = holders
            .iter()
            .copied()
            .filter(|objective| {
                self.holds
                    .binary_search_by_key(objective, |row| row.objective)
                    .is_err()
            })
            .collect();
        new_holds.sort_unstable();
        new_holds.dedup();
        let mut new_scores: Vec<u8> = scores
            .iter()
            .copied()
            .filter(|team| self.scores.binary_search_by_key(team, |row| row.team).is_err())
            .collect();
        new_scores.sort_unstable();
        new_scores.dedup();
        let fits = |held: usize, new: usize| held.saturating_add(new) <= MAX_REPORT_ROWS;
        if fits(self.tagged.len(), new_tags.len())
            && fits(self.holds.len(), new_holds.len())
            && fits(self.scores.len(), new_scores.len())
        {
            Ok(())
        } else {
            Err(RulesError::VictorySettings { field: "rows" })
        }
    }

    /// Record that `team` (`0` for nobody) holds `objective` from match tick `tick`. The same
    /// holder keeps its start tick.
    pub fn report_holder(&mut self, objective: u32, team: u8, tick: u64) -> Result<(), RulesError> {
        match self.holds.binary_search_by_key(&objective, |row| row.objective) {
            Ok(at) => {
                let row = &mut self.holds[at];
                if row.holder != team {
                    row.holder = team;
                    row.since = tick;
                }
            }
            Err(at) => {
                check_rows(self.holds.len())?;
                self.holds.insert(
                    at,
                    HoldRow {
                        objective,
                        holder: team,
                        since: tick,
                    },
                );
            }
        }
        Ok(())
    }

    /// Record `team`'s score.
    pub fn report_score(&mut self, team: u8, score: i64) -> Result<(), RulesError> {
        match self.scores.binary_search_by_key(&team, |row| row.team) {
            Ok(at) => self.scores[at].score = score,
            Err(at) => {
                check_rows(self.scores.len())?;
                self.scores.insert(at, ScoreRow { team, score });
            }
        }
        Ok(())
    }

    /// Teams in `standing` that reported some units of `tag` and now have none.
    pub fn stripped(&self, tag: &str, standing: &[u8]) -> Vec<u8> {
        standing
            .iter()
            .copied()
            .filter(|team| {
                self.tagged
                    .iter()
                    .any(|row| row.team == *team && row.tag == tag && row.seen && row.count == 0)
            })
            .collect()
    }

    /// The team that has held `objective` for `ticks` at match tick `tick`, if any.
    pub fn held_long_enough(&self, objective: u32, ticks: u64, tick: u64) -> Option<u8> {
        let row = self.holds.iter().find(|row| row.objective == objective)?;
        (row.holder != 0 && tick.saturating_sub(row.since) >= ticks).then_some(row.holder)
    }

    /// The team in `standing` with the best score at or past `threshold` (ties: the lower id).
    pub fn score_leader(&self, threshold: i64, standing: &[u8]) -> Option<u8> {
        let mut best: Option<ScoreRow> = None;
        for row in &self.scores {
            if row.score < threshold || !standing.contains(&row.team) {
                continue;
            }
            // Rows are sorted by team, so strictly greater keeps the lower id on a tie.
            if best.is_none_or(|b| row.score > b.score) {
                best = Some(*row);
            }
        }
        best.map(|row| row.team)
    }

    /// The tick `team` may vote again, when it is cooling down at `tick`.
    pub fn cooldown_until(&self, team: u8, tick: u64) -> Option<u64> {
        self.cooldowns
            .iter()
            .find(|c| c.team == team && tick < c.until)
            .map(|c| c.until)
    }

    /// Record `seat`'s surrender vote at match tick `tick`. The first vote opens the team's
    /// ballot; a second vote of the same seat replaces its first. A team cooling down is refused
    /// with [`RulesError::SurrenderCooldown`].
    pub fn cast(&mut self, seat: SeatId, yes: bool, tick: u64) -> Result<(), RulesError> {
        if let Some(until) = self.cooldown_until(seat.team, tick) {
            return Err(RulesError::SurrenderCooldown {
                team: seat.team,
                until,
            });
        }
        let at = match self.ballots.binary_search_by_key(&seat.team, |b| b.team) {
            Ok(at) => at,
            Err(at) => {
                self.ballots.insert(
                    at,
                    SurrenderBallot {
                        team: seat.team,
                        opened: tick,
                        yes: Vec::new(),
                        no: Vec::new(),
                    },
                );
                at
            }
        };
        let ballot = &mut self.ballots[at];
        let (add, remove) = if yes {
            (&mut ballot.yes, &mut ballot.no)
        } else {
            (&mut ballot.no, &mut ballot.yes)
        };
        if let Ok(i) = remove.binary_search(&seat.slot) {
            remove.remove(i);
        }
        if let Err(i) = add.binary_search(&seat.slot) {
            add.insert(i, seat.slot);
        }
        Ok(())
    }

    /// Resolve the open ballots at match tick `tick`. `voters(team)` is the team's voting slots,
    /// sorted. Returns the teams whose vote passed; failed ballots start their team's cooldown and
    /// push [`RulesEvent::SurrenderFailed`].
    pub fn resolve<V>(
        &mut self,
        rules: &SurrenderRules,
        tick: u64,
        mut voters: V,
        events: &mut Vec<RulesEvent>,
    ) -> Vec<u8>
    where
        V: FnMut(u8) -> Vec<u8>,
    {
        let mut passed = Vec::new();
        let mut failed = Vec::new();
        for ballot in &self.ballots {
            let slots = voters(ballot.team);
            let need = rules.needed(slots.len());
            let yes = ballot
                .yes
                .iter()
                .filter(|s| slots.binary_search(s).is_ok())
                .count();
            let no = ballot
                .no
                .iter()
                .filter(|s| slots.binary_search(s).is_ok())
                .count();
            if yes >= need && !slots.is_empty() {
                passed.push(ballot.team);
            } else if slots.len().saturating_sub(no) < need
                || tick.saturating_sub(ballot.opened) >= rules.window_ticks
                || slots.is_empty()
            {
                failed.push(ballot.team);
            }
        }
        for team in &failed {
            let until = tick.saturating_add(rules.cooldown_ticks);
            match self.cooldowns.binary_search_by_key(team, |c| c.team) {
                Ok(at) => self.cooldowns[at].until = until,
                Err(at) => self
                    .cooldowns
                    .insert(at, SurrenderCooldown { team: *team, until }),
            }
            events.push(RulesEvent::SurrenderFailed {
                tick,
                team: *team,
                until,
            });
        }
        self.ballots
            .retain(|b| !passed.contains(&b.team) && !failed.contains(&b.team));
        passed
    }
}

/// The `strategy_default` rule set (until the rule-set format lands): a team that loses its last
/// `capital` (host-reported) is out, teams may surrender with the default vote, and the last side
/// standing wins. Respawn and the flow keep their defaults.
pub fn strategy_default_settings() -> crate::session::SessionSettings {
    crate::session::SessionSettings {
        victory: VictoryRules {
            conditions: vec![
                VictoryCondition::DestroyAllTagged {
                    tag: "capital".to_string(),
                },
                VictoryCondition::Surrender,
            ],
            surrender: SurrenderRules::default(),
        },
        ..crate::session::SessionSettings::default()
    }
}

fn check_rows(len: usize) -> Result<(), RulesError> {
    if len >= MAX_REPORT_ROWS {
        Err(RulesError::VictorySettings { field: "rows" })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-8802: a batch is checked whole: updates add nothing, a new key listed twice counts
    /// once, and one new key past the cap refuses the batch.
    #[test]
    fn check_report_counts_new_keys_once_against_the_cap() {
        let mut state = VictoryState::default();
        for i in 0..MAX_REPORT_ROWS - 1 {
            state.report_tagged(1, &format!("t{i:04}"), 1).expect("fill");
        }
        assert_eq!(
            state.check_report(&[(1, "t0000"), (1, "new"), (1, "new")], &[], &[]),
            Ok(())
        );
        assert_eq!(
            state.check_report(&[(1, "new"), (2, "new")], &[], &[]),
            Err(RulesError::VictorySettings { field: "rows" })
        );
        assert_eq!(
            state.check_report(&[(1, "")], &[], &[]),
            Err(RulesError::VictorySettings { field: "tag" })
        );
        assert_eq!(state.check_report(&[], &[7, 7], &[1, 1]), Ok(()));
        state.report_tagged(1, "new", 1).expect("the last row fits");
        assert_eq!(
            state.check_report(&[(1, "t0000"), (1, "other")], &[], &[]),
            Err(RulesError::VictorySettings { field: "rows" })
        );
        assert_eq!(state.check_report(&[(1, "t0000")], &[], &[]), Ok(()));
    }

    #[test]
    fn the_strategy_default_set_validates() {
        let set = strategy_default_settings();
        assert!(set.victory.validate().is_ok());
        assert!(set.victory.has("destroy_all_tagged"));
        assert!(set.victory.has("surrender"));
        assert!(set.zone.is_none());
    }

    #[test]
    fn needed_rounds_up_and_is_at_least_one() {
        let mut rules = SurrenderRules::default();
        assert_eq!(rules.needed(4), 4);
        rules.threshold_percent = 50;
        assert_eq!(rules.needed(3), 2);
        assert_eq!(rules.needed(1), 1);
        rules.threshold_percent = 0;
        assert_eq!(rules.needed(4), 1);
        assert_eq!(rules.needed(0), 1);
    }

    #[test]
    fn reports_keep_sorted_rows_and_holders_keep_their_start() {
        let mut state = VictoryState::default();
        state.report_tagged(2, "capital", 1).expect("tag");
        state.report_tagged(1, "capital", 0).expect("tag");
        assert_eq!(state.tagged[0].team, 1);
        assert!(!state.tagged[0].seen, "never had one");
        assert_eq!(
            state.report_tagged(1, "", 1),
            Err(RulesError::VictorySettings { field: "tag" })
        );
        state.report_holder(7, 3, 10).expect("hold");
        state.report_holder(7, 3, 20).expect("hold");
        assert_eq!(state.holds[0].since, 10, "the same holder keeps its start");
        state.report_score(4, 9).expect("score");
        state.report_score(2, 9).expect("score");
        assert_eq!(
            state.score_leader(9, &[2, 4]),
            Some(2),
            "a tie goes to the lower id"
        );
        assert_eq!(state.score_leader(10, &[2, 4]), None);
    }
}
