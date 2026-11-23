// SPDX-License-Identifier: MPL-2.0
//! Co-op PvE waves (audit gap GR-11.1): a wave table, a deterministic director that releases
//! enemy groups as the alive count allows, a shared team lives pool, and the co-op fail and
//! success conditions.
//!
//! The core never names a prefab. A [`WaveGroup`] carries an archetype id and a spawn tag; the
//! director sends [`RulesEvent::WaveSpawn`] with a count, the game spawns that many of the
//! archetype at points with that tag, tags each enemy with the wave index, and reports each
//! enemy death with [`crate::session::MatchSession::note_wave_down`]. The alive count is
//! `released - reported down`, so it does not depend on when the game's entities appear.
//!
//! All state is integers in sorted vectors. A session with an empty wave table (the default)
//! never touches this module, so S-14's event stream and session bits are unchanged.

use serde::{Deserialize, Serialize};

use crate::RulesError;
use crate::objective::RulesEvent;
use crate::win_condition::Outcome;

/// Most rows a wave table holds.
pub const MAX_WAVES: usize = 256;

/// Most groups one wave holds.
pub const MAX_GROUPS: usize = 64;

/// Longest archetype id or spawn tag, in bytes.
pub const MAX_ID_BYTES: usize = 64;

/// Most difficulty rows.
pub const MAX_DIFFICULTY_ROWS: usize = 64;

/// Highest difficulty percentage (ten times the table's counts).
pub const MAX_PERCENT: u32 = 1000;

/// When a wave starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaveStart {
    /// `delay_ticks` match ticks after the previous wave is cleared. The first wave counts from
    /// the first step the director runs in the match.
    AfterCleared {
        /// Ticks to wait.
        #[serde(default)]
        delay_ticks: u64,
    },
    /// On the first step at or after this match tick, whether or not earlier waves are cleared.
    /// Waves still start in table order: a row never starts before the row above it.
    AtTick {
        /// The match tick.
        tick: u64,
    },
}

impl Default for WaveStart {
    fn default() -> Self {
        Self::AfterCleared { delay_ticks: 0 }
    }
}

/// One group of a wave: `count` enemies of one archetype at points with one spawn tag.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WaveGroup {
    /// The game's archetype id (a prefab key, an enemy class). The core only copies it.
    pub archetype: String,
    /// Enemies at 100 % difficulty. At least 1.
    pub count: u32,
    /// The spawn tag the game picks points by. May be empty.
    pub spawn_tag: String,
}

/// One row of the wave table.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WaveRow {
    /// When the wave starts.
    pub at: WaveStart,
    /// Its groups, released in order: a later group waits until every earlier one is out.
    pub groups: Vec<WaveGroup>,
    /// Most enemies alive at once (all waves together) while this wave releases. `0` is no cap.
    pub alive_cap: u32,
}

/// Difficulty for a seated player count: a percentage applied to every group count and alive cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DifficultyRow {
    /// The row applies from this many seated players up to the next row's count.
    pub players: u32,
    /// Percentage of each group's count (rounded down, at least 1).
    pub count_percent: u32,
    /// Percentage of each wave's alive cap (rounded down, at least 1; a `0` cap stays no cap).
    pub cap_percent: u32,
}

/// What wins a co-op match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoopSuccess {
    /// The last wave is cleared.
    #[default]
    LastWaveCleared,
    /// The host reports the objective reached ([`CoopObjective::Reached`]).
    ObjectiveReached,
    /// Whichever comes first.
    Either,
}

/// A co-op objective report from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoopObjective {
    /// The objective the players defend was destroyed: the match fails.
    Destroyed,
    /// The players reached the objective: the match is won when the success rule counts it.
    Reached,
}

/// Why a co-op match ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoopCause {
    /// Every wave was cleared.
    AllWavesCleared,
    /// The host reported the objective reached.
    ObjectiveReached,
    /// Every seated player on the players' team was down at once (eliminated, or waiting to
    /// respawn after this tick).
    TeamWipe,
    /// The host reported the objective destroyed.
    ObjectiveDestroyed,
}

impl CoopCause {
    /// Lowercase word (`"all_waves_cleared"`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AllWavesCleared => "all_waves_cleared",
            Self::ObjectiveReached => "objective_reached",
            Self::TeamWipe => "team_wipe",
            Self::ObjectiveDestroyed => "objective_destroyed",
        }
    }

    /// True for the two success causes.
    pub fn is_win(self) -> bool {
        matches!(self, Self::AllWavesCleared | Self::ObjectiveReached)
    }
}

/// Co-op wave settings (`SessionSettings::waves`). An empty `waves` list is off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WaveSettings {
    /// The wave table, played in order.
    pub waves: Vec<WaveRow>,
    /// Difficulty by seated player count, ascending by `players`. Empty is 100 %.
    pub difficulty: Vec<DifficultyRow>,
    /// The players' team. Enemies are the game's; they hold no seat.
    pub players_team: u8,
    /// Lives the players' team shares. `None` is off (each seat follows its respawn policy). A
    /// death spends one life and respawns by the policy; a death with the pool empty eliminates.
    pub shared_lives: Option<u32>,
    /// Fail when every seated player on the players' team is down at once.
    pub fail_on_team_wipe: bool,
    /// What wins.
    pub success: CoopSuccess,
}

impl Default for WaveSettings {
    fn default() -> Self {
        Self {
            waves: Vec::new(),
            difficulty: Vec::new(),
            players_team: 1,
            shared_lives: None,
            fail_on_team_wipe: true,
            success: CoopSuccess::LastWaveCleared,
        }
    }
}

fn bad(reason: &'static str) -> RulesError {
    RulesError::WaveSettings { reason }
}

impl WaveSettings {
    /// True when the wave table is empty (the director never runs).
    pub fn is_off(&self) -> bool {
        self.waves.is_empty()
    }

    /// Refuse settings the director cannot play ([`RulesError::WaveSettings`]).
    pub fn validate(&self) -> Result<(), RulesError> {
        if self.waves.len() > MAX_WAVES {
            return Err(bad("too_many_waves"));
        }
        if self.players_team == 0 {
            return Err(bad("players_team_zero"));
        }
        for row in &self.waves {
            if row.groups.is_empty() {
                return Err(bad("empty_wave"));
            }
            if row.groups.len() > MAX_GROUPS {
                return Err(bad("too_many_groups"));
            }
            for group in &row.groups {
                if group.count == 0 {
                    return Err(bad("zero_count"));
                }
                if group.archetype.is_empty() || group.archetype.len() > MAX_ID_BYTES {
                    return Err(bad("archetype_length"));
                }
                if group.spawn_tag.len() > MAX_ID_BYTES {
                    return Err(bad("spawn_tag_length"));
                }
            }
        }
        if self.difficulty.len() > MAX_DIFFICULTY_ROWS {
            return Err(bad("too_many_difficulty_rows"));
        }
        let mut last: Option<u32> = None;
        for row in &self.difficulty {
            if last.is_some_and(|players| row.players <= players) {
                return Err(bad("difficulty_not_ascending"));
            }
            if row.count_percent == 0
                || row.cap_percent == 0
                || row.count_percent > MAX_PERCENT
                || row.cap_percent > MAX_PERCENT
            {
                return Err(bad("difficulty_percent"));
            }
            last = Some(row.players);
        }
        Ok(())
    }

    /// The difficulty row for `players` seated players: the row with the largest `players` not
    /// above the count. `None` (100 %) when no row applies.
    pub fn difficulty_for(&self, players: u32) -> Option<DifficultyRow> {
        self.difficulty
            .iter()
            .rev()
            .find(|row| row.players <= players)
            .copied()
    }
}

fn scale(value: u32, percent: u32) -> u32 {
    if value == 0 {
        return 0;
    }
    let scaled = u64::from(value) * u64::from(percent) / 100;
    u32::try_from(scaled).unwrap_or(u32::MAX).max(1)
}

/// One started wave.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WaveProgress {
    /// Index into the wave table.
    pub wave: u32,
    /// Match tick it started.
    pub started_at: u64,
    /// Enemies per group after difficulty scaling (fixed when the wave starts).
    pub planned: Vec<u32>,
    /// Enemies released per group so far.
    pub released: Vec<u32>,
    /// The wave's alive cap after scaling (`0` is no cap).
    pub alive_cap: u32,
    /// Released and not yet reported down.
    pub alive: u32,
    /// Match tick it was cleared.
    pub cleared_at: Option<u64>,
}

impl WaveProgress {
    fn all_released(&self) -> bool {
        self.planned == self.released
    }
}

/// The director's state for the running match. A launch (and a round reset) clears it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WaveState {
    /// Match tick of the first step the director ran in this match.
    pub opened_at: Option<u64>,
    /// Started waves, in start order (which is table order).
    pub progress: Vec<WaveProgress>,
    /// Lives left in the shared pool (`None` when the pool is off).
    pub lives_left: Option<u32>,
    /// The host reported the objective destroyed.
    pub objective_destroyed: bool,
    /// The host reported the objective reached.
    pub objective_reached: bool,
    /// How the match ended, once it has.
    pub ended: Option<CoopEnd>,
}

/// How a co-op match ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoopEnd {
    /// Match tick.
    pub tick: u64,
    /// Why.
    pub cause: CoopCause,
}

impl WaveState {
    /// Enemies released and not reported down, over every wave.
    pub fn alive_total(&self) -> u32 {
        self.progress
            .iter()
            .fold(0u32, |sum, wave| sum.saturating_add(wave.alive))
    }

    /// Waves cleared so far.
    pub fn cleared(&self) -> u32 {
        let n = self
            .progress
            .iter()
            .filter(|wave| wave.cleared_at.is_some())
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }
}

/// What the session tells the director on one step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoopView {
    /// The match tick of this step.
    pub tick: u64,
    /// Seated, playing seats on the players' team.
    pub players: u32,
    /// How many of them are down: eliminated, or waiting to respawn after this tick.
    pub players_down: u32,
}

/// Wave settings and the running state, as the session holds them.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CoopWaves {
    /// The settings.
    pub settings: WaveSettings,
    /// The running match's state.
    pub state: WaveState,
}

impl CoopWaves {
    /// Waves with `settings` and a fresh state.
    pub fn new(settings: WaveSettings) -> Self {
        let mut out = Self {
            settings,
            state: WaveState::default(),
        };
        out.reset();
        out
    }

    /// True when the wave table is empty.
    pub fn is_off(&self) -> bool {
        self.settings.is_off()
    }

    /// Forget the running match (a launch or a round reset). The lives pool refills.
    pub fn reset(&mut self) {
        self.state = WaveState {
            lives_left: self.settings.shared_lives,
            ..WaveState::default()
        };
    }

    /// `count` enemies of wave `wave` died. An unknown wave, or more than are alive, is clamped.
    pub fn note_down(&mut self, wave: u32, count: u32) {
        if let Some(progress) = self.state.progress.iter_mut().find(|p| p.wave == wave) {
            progress.alive = progress.alive.saturating_sub(count);
        }
    }

    /// Record a co-op objective report.
    pub fn note_objective(&mut self, objective: CoopObjective) {
        match objective {
            CoopObjective::Destroyed => self.state.objective_destroyed = true,
            CoopObjective::Reached => self.state.objective_reached = true,
        }
    }

    /// Spend one shared life for a death of the players' team. `true` when the pool is off or a
    /// life was spent (the seat respawns by its policy); `false` when the pool is empty (the
    /// seat is eliminated).
    pub fn spend_life(&mut self) -> bool {
        match self.state.lives_left {
            None => true,
            Some(0) => false,
            Some(left) => {
                self.state.lives_left = Some(left - 1);
                true
            }
        }
    }

    /// Run the director for one step: start due waves, release groups under the alive caps,
    /// mark cleared waves, then evaluate the fail and success conditions. Events go to `push`.
    pub fn step(&mut self, view: CoopView, push: &mut dyn FnMut(RulesEvent)) {
        if self.is_off() || self.state.ended.is_some() {
            return;
        }
        let tick = view.tick;
        let opened = *self.state.opened_at.get_or_insert(tick);
        self.start_due(tick, opened, view.players, push);
        self.release(tick, push);
        self.mark_cleared(tick, push);
        if let Some(cause) = self.decide(view) {
            self.state.ended = Some(CoopEnd { tick, cause });
            push(RulesEvent::CoopEnded {
                tick,
                won: cause.is_win(),
                cause,
            });
        }
    }

    fn start_due(&mut self, tick: u64, opened: u64, players: u32, push: &mut dyn FnMut(RulesEvent)) {
        while let Some(row) = self.settings.waves.get(self.state.progress.len()) {
            let index = self.state.progress.len();
            let due = match row.at {
                WaveStart::AtTick { tick: at } => Some(at),
                WaveStart::AfterCleared { delay_ticks } => {
                    if index == 0 {
                        Some(opened.saturating_add(delay_ticks))
                    } else {
                        self.state.progress[index - 1]
                            .cleared_at
                            .map(|cleared| cleared.saturating_add(delay_ticks))
                    }
                }
            };
            if !due.is_some_and(|due| due <= tick) {
                break;
            }
            let difficulty = self.settings.difficulty_for(players);
            let (count_pct, cap_pct) = difficulty.map_or((100, 100), |d| (d.count_percent, d.cap_percent));
            let planned: Vec<u32> = row.groups.iter().map(|g| scale(g.count, count_pct)).collect();
            let wave = u32::try_from(index).unwrap_or(u32::MAX);
            self.state.progress.push(WaveProgress {
                wave,
                started_at: tick,
                released: vec![0; planned.len()],
                planned,
                alive_cap: scale(row.alive_cap, cap_pct),
                alive: 0,
                cleared_at: None,
            });
            push(RulesEvent::WaveStarted { tick, wave });
        }
    }

    fn release(&mut self, tick: u64, push: &mut dyn FnMut(RulesEvent)) {
        let mut alive_total = self.state.alive_total();
        for progress in &mut self.state.progress {
            if progress.cleared_at.is_some() {
                continue;
            }
            let Some(row) = self.settings.waves.get(progress.wave as usize) else {
                continue;
            };
            for (group_index, group) in row.groups.iter().enumerate() {
                let planned = progress.planned.get(group_index).copied().unwrap_or(0);
                let released = progress.released.get(group_index).copied().unwrap_or(0);
                let remaining = planned.saturating_sub(released);
                if remaining == 0 {
                    continue;
                }
                let room = if progress.alive_cap == 0 {
                    remaining
                } else {
                    progress.alive_cap.saturating_sub(alive_total)
                };
                let n = remaining.min(room);
                if n == 0 {
                    // Groups go out in order: a later group never jumps a held one.
                    break;
                }
                if let Some(slot) = progress.released.get_mut(group_index) {
                    *slot = released + n;
                }
                progress.alive = progress.alive.saturating_add(n);
                alive_total = alive_total.saturating_add(n);
                push(RulesEvent::WaveSpawn {
                    tick,
                    wave: progress.wave,
                    group: u32::try_from(group_index).unwrap_or(u32::MAX),
                    archetype: group.archetype.clone(),
                    count: n,
                    spawn_tag: group.spawn_tag.clone(),
                });
                if n < remaining {
                    break;
                }
            }
        }
    }

    fn mark_cleared(&mut self, tick: u64, push: &mut dyn FnMut(RulesEvent)) {
        for progress in &mut self.state.progress {
            if progress.cleared_at.is_none() && progress.all_released() && progress.alive == 0 {
                progress.cleared_at = Some(tick);
                push(RulesEvent::WaveCleared {
                    tick,
                    wave: progress.wave,
                });
            }
        }
    }

    /// Fail conditions first (objective destroyed, team wipe), then success.
    fn decide(&self, view: CoopView) -> Option<CoopCause> {
        if self.state.objective_destroyed {
            return Some(CoopCause::ObjectiveDestroyed);
        }
        if self.settings.fail_on_team_wipe && view.players > 0 && view.players_down >= view.players {
            return Some(CoopCause::TeamWipe);
        }
        let waves_done = self.state.progress.len() == self.settings.waves.len()
            && self.state.progress.iter().all(|p| p.cleared_at.is_some());
        let by_waves = matches!(
            self.settings.success,
            CoopSuccess::LastWaveCleared | CoopSuccess::Either
        ) && waves_done;
        let by_objective = matches!(
            self.settings.success,
            CoopSuccess::ObjectiveReached | CoopSuccess::Either
        ) && self.state.objective_reached;
        if by_waves {
            Some(CoopCause::AllWavesCleared)
        } else if by_objective {
            Some(CoopCause::ObjectiveReached)
        } else {
            None
        }
    }

    /// The match outcome once the director has decided it: the players' team on a win, `0` on a
    /// failure. Rule word `"waves"`.
    pub fn outcome(&self) -> Option<Outcome> {
        let end = self.state.ended?;
        Some(Outcome {
            winner: if end.cause.is_win() {
                self.settings.players_team
            } else {
                0
            },
            rule: "waves",
        })
    }
}

/// The co-op result as a [`crate::win_condition::WinCondition`]: it reads the director, not the
/// view, so a host that evaluates several rules can list it beside the stock ones.
#[derive(Debug, Clone, Copy)]
pub struct WavesWin<'a>(pub &'a CoopWaves);

impl crate::win_condition::WinCondition for WavesWin<'_> {
    fn evaluate(&self, _view: &dyn crate::win_condition::MatchView) -> Option<Outcome> {
        self.0.outcome()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(archetype: &str, count: u32) -> WaveGroup {
        WaveGroup {
            archetype: archetype.into(),
            count,
            spawn_tag: "east".into(),
        }
    }

    #[test]
    fn difficulty_row_picks_the_largest_players_not_above_the_count() {
        let settings = WaveSettings {
            difficulty: vec![
                DifficultyRow {
                    players: 1,
                    count_percent: 100,
                    cap_percent: 100,
                },
                DifficultyRow {
                    players: 3,
                    count_percent: 150,
                    cap_percent: 125,
                },
            ],
            ..WaveSettings::default()
        };
        assert_eq!(settings.difficulty_for(0), None);
        assert_eq!(settings.difficulty_for(2).map(|d| d.count_percent), Some(100));
        assert_eq!(settings.difficulty_for(4).map(|d| d.count_percent), Some(150));
        assert_eq!(scale(5, 150), 7);
        assert_eq!(scale(1, 10), 1, "a scaled count never drops to 0");
        assert_eq!(scale(0, 150), 0, "no cap stays no cap");
    }

    #[test]
    fn validate_refuses_empty_waves_and_unsorted_difficulty() {
        let mut settings = WaveSettings {
            waves: vec![WaveRow::default()],
            ..WaveSettings::default()
        };
        assert_eq!(settings.validate(), Err(bad("empty_wave")));
        settings.waves[0].groups.push(group("grunt", 0));
        assert_eq!(settings.validate(), Err(bad("zero_count")));
        settings.waves[0].groups[0].count = 2;
        assert_eq!(settings.validate(), Ok(()));
        settings.difficulty = vec![
            DifficultyRow {
                players: 2,
                count_percent: 100,
                cap_percent: 100,
            },
            DifficultyRow {
                players: 2,
                count_percent: 100,
                cap_percent: 100,
            },
        ];
        assert_eq!(settings.validate(), Err(bad("difficulty_not_ascending")));
    }

    #[test]
    fn shared_lives_spend_until_empty() {
        let mut waves = CoopWaves::new(WaveSettings {
            shared_lives: Some(2),
            ..WaveSettings::default()
        });
        assert!(waves.spend_life());
        assert!(waves.spend_life());
        assert!(!waves.spend_life(), "an empty pool eliminates");
        waves.reset();
        assert_eq!(waves.state.lives_left, Some(2), "a reset refills the pool");
    }
}
