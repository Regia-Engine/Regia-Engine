// SPDX-License-Identifier: MPL-2.0
//! Capture zones (ADR-8649): control volumes with integer capture progress, an owner, decay, a
//! contest rule by numbers and owner scoring, plus the three modes built on them: domination (every
//! zone at once), hardpoint (one active zone from a rotation list) and attack/defend (zones unlock
//! in order).
//!
//! A zone is an [`Objective`] of kind [`ObjectiveKind::Zone`] whose [`Objective::zone`] holds its
//! [`ZoneSettings`] and its state. [`ObjectiveBoard::zones_tick`] steps every zone once per match
//! tick: it counts the living units of each fighting team inside the volume, so presence between
//! two ticks always counts (king of the hill samples once per period and is unchanged).
//!
//! # The step, per zone
//!
//! Zones run in ascending entity id. Units are counted per team, so the host's unit order does not
//! change the result; a unit id listed twice counts once. A locked zone does nothing. Otherwise the [`Contest`] rule turns the counts
//! into one of three cases:
//!
//! - **empty** (no living fighting unit inside): with [`ZoneSettings::decay_ticks`] set, partial
//!   progress loses one unit every `decay_ticks` empty ticks; at zero the zone has no capturing
//!   team. The owner keeps the zone.
//! - **contested**: nothing moves.
//! - **pushed** by team `T` at rate `r` (at least 1, at most [`ZoneSettings::max_rate_units`]):
//!   - `T` owns the zone: another team's partial progress unwinds by `r`;
//!   - another team has partial progress: it unwinds by `r` first (no carry-over on that tick);
//!   - otherwise `T` becomes the capturing team and progress grows by `r`. When it reaches
//!     [`ZoneSettings::capture_ticks`] the zone flips: `T` owns it, progress returns to zero and a
//!     [`RulesEvent::ZoneCaptured`] carries the previous owner (`0` for neutral).
//!
//! Then an owned, unlocked zone counts one held tick; every
//! [`ZoneSettings::owner_scores_every`] held ticks its owner gains a team point
//! ([`RulesEvent::ZonePoint`]). [`ZoneRules::score_limit`] ends the match on that point
//! ([`EndReason::ZoneScoreLimitReached`]).
//!
//! Every unlocked zone takes the tick, even after one of them reached the score limit or took the
//! last attack/defend zone; the match then ends once, after the pass, for the first such zone in
//! entity-id order (so a round end the phase machine reopens never skips a later zone's tick).
//!
//! Everything is integer `+ - *` and comparisons over sorted vectors; `cargo xtask det-scan` scans
//! this file.

use serde::{Deserialize, Serialize};

use super::RulesError;
use super::objective::{
    Contestant, EndReason, HillVolume, Objective, ObjectiveBoard, ObjectiveKind, RulesEvent,
};

/// Default capture time of a zone: ten seconds of one unit at the default tick rate.
pub const DEFAULT_ZONE_CAPTURE_TICKS: u64 = 600;

/// How a zone with more than one team inside decides who pushes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Contest {
    /// Any enemy unit inside freezes the zone (the default).
    #[default]
    AnyEnemyBlocks,
    /// The team with strictly more units than every other team pushes at its own count (capped);
    /// a tie for the most freezes the zone.
    Majority,
    /// The largest team pushes at its count minus the units of every other team inside (capped);
    /// zero or less freezes the zone.
    Weighted,
}

impl Contest {
    /// The rule as a lowercase string (`"any_enemy_blocks"`, `"majority"`, `"weighted"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AnyEnemyBlocks => "any_enemy_blocks",
            Self::Majority => "majority",
            Self::Weighted => "weighted",
        }
    }

    /// Who pushes the zone, from the living units per team inside (`counts[team]`, index 0
    /// unused), with the rate capped at `max_rate` (never below 1).
    pub fn resolve(self, counts: &[u32], max_rate: u32) -> Presence {
        let cap = max_rate.max(1);
        let mut top: Option<(u8, u32)> = None;
        let mut second = 0u32;
        let mut total = 0u32;
        let mut teams = 0u32;
        for (team, &count) in counts.iter().enumerate().skip(1) {
            if count == 0 {
                continue;
            }
            let Ok(team) = u8::try_from(team) else {
                continue;
            };
            teams += 1;
            total = total.saturating_add(count);
            match top {
                Some((_, best)) if count <= best => second = second.max(count),
                Some((_, best)) => {
                    second = best;
                    top = Some((team, count));
                }
                None => top = Some((team, count)),
            }
        }
        let Some((team, count)) = top else {
            return Presence::Empty;
        };
        if teams == 1 {
            return Presence::Pushed {
                team,
                rate: count.min(cap),
            };
        }
        match self {
            Self::AnyEnemyBlocks => Presence::Contested,
            Self::Majority if count > second => Presence::Pushed {
                team,
                rate: count.min(cap),
            },
            Self::Majority => Presence::Contested,
            Self::Weighted => {
                let others = total - count;
                if count > others {
                    Presence::Pushed {
                        team,
                        rate: (count - others).min(cap),
                    }
                } else {
                    Presence::Contested
                }
            }
        }
    }
}

/// What a zone's contest rule made of the units inside on one tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// No living fighting unit inside.
    Empty,
    /// More than one team inside and none pushes.
    Contested,
    /// `team` pushes at `rate` progress units this tick.
    Pushed {
        /// The pushing team.
        team: u8,
        /// Progress units this tick (`1..=max_rate_units`).
        rate: u32,
    },
}

/// A capture zone's settings. Every value is a match-tick or unit count.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ZoneSettings {
    /// The control volume (a box with a height band, as a hill).
    pub volume: HillVolume,
    /// Progress units a capture needs (one unit pushing for one tick adds one); `0` reads as `1`.
    pub capture_ticks: u64,
    /// An empty zone's partial progress loses one unit every `decay_ticks` empty ticks (`0` reads
    /// as `1`). `None` holds partial progress while the zone is empty.
    pub decay_ticks: Option<u64>,
    /// How a zone with several teams inside decides who pushes.
    pub contest: Contest,
    /// The most units that speed a capture up; `0` reads as `1`.
    pub max_rate_units: u32,
    /// The owner gains one team point every this many ticks it holds the zone (`0` reads as `1`).
    /// `None` scores nothing for holding.
    pub owner_scores_every: Option<u32>,
    /// The zone starts locked: it takes no progress and scores nothing until a zone rule unlocks
    /// it.
    pub locked: bool,
}

impl ZoneSettings {
    /// Settings for `volume` with the defaults: [`DEFAULT_ZONE_CAPTURE_TICKS`], no decay, any enemy
    /// blocks, one unit of rate, one point per second held, unlocked.
    pub fn new(volume: HillVolume) -> Self {
        Self {
            volume,
            capture_ticks: DEFAULT_ZONE_CAPTURE_TICKS,
            decay_ticks: None,
            contest: Contest::AnyEnemyBlocks,
            max_rate_units: 1,
            owner_scores_every: Some(regia_detsim::tick::TICK_RATE_HZ),
            locked: false,
        }
    }

    /// The capture time in force (never below 1).
    pub fn effective_capture_ticks(&self) -> u64 {
        self.capture_ticks.max(1)
    }
}

/// A capture zone: its settings and state. Lives in [`Objective::zone`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Zone {
    /// The settings it was installed with.
    pub settings: ZoneSettings,
    /// The owning team; `0` while neutral.
    pub owner: u8,
    /// The team whose partial progress the zone holds; `0` for none.
    pub capturing_team: u8,
    /// Partial capture progress of [`Zone::capturing_team`], `0..capture_ticks`.
    pub progress: u64,
    /// Locked now (a locked zone takes no progress and scores nothing).
    pub locked: bool,
    /// Ticks the owner has held it unlocked since it flipped (the owner-scoring counter).
    pub held_ticks: u64,
    /// Consecutive empty ticks (the decay counter).
    pub empty_ticks: u64,
}

impl Zone {
    /// A neutral zone with `settings`, locked when the settings say so.
    pub fn new(settings: ZoneSettings) -> Self {
        Self {
            settings,
            owner: 0,
            capturing_team: 0,
            progress: 0,
            locked: settings.locked,
            held_ticks: 0,
            empty_ticks: 0,
        }
    }

    /// Back to neutral with no progress, locked as installed.
    pub fn reset(&mut self) {
        *self = Self::new(self.settings);
    }

    /// One tick of the progress rule for `presence`. Returns the previous owner when the zone
    /// flipped.
    fn step(&mut self, presence: Presence) -> Option<u8> {
        match presence {
            Presence::Empty => {
                self.empty_ticks = self.empty_ticks.saturating_add(1);
                if let Some(period) = self.settings.decay_ticks
                    && self.progress > 0
                    && self.empty_ticks.is_multiple_of(period.max(1))
                {
                    self.unwind(1);
                }
                None
            }
            Presence::Contested => {
                self.empty_ticks = 0;
                None
            }
            Presence::Pushed { team, rate } => {
                self.empty_ticks = 0;
                let rate = u64::from(rate);
                if team == self.owner || (self.capturing_team != 0 && self.capturing_team != team) {
                    self.unwind(rate);
                    return None;
                }
                self.capturing_team = team;
                let need = self.settings.effective_capture_ticks();
                self.progress = self.progress.saturating_add(rate).min(need);
                if self.progress < need {
                    return None;
                }
                let previous = self.owner;
                self.owner = team;
                self.capturing_team = 0;
                self.progress = 0;
                self.held_ticks = 0;
                Some(previous)
            }
        }
    }

    fn unwind(&mut self, by: u64) {
        self.progress = self.progress.saturating_sub(by);
        if self.progress == 0 {
            self.capturing_team = 0;
        }
    }
}

/// How the board's zones play together.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ZoneMode {
    /// Every zone as installed (its own lock flag, restored when this mode is set); owners score
    /// per zone. The default.
    #[default]
    Domination,
    /// One active zone: `rotation[active]` is unlocked, the other zones of the list are locked.
    /// Every `rotate_ticks` zone ticks the next zone of the list (wrapping) activates and the old
    /// one locks and goes back to neutral.
    Hardpoint {
        /// Zone ids in rotation order.
        rotation: Vec<u32>,
        /// Zone ticks each zone stays active (at least 1).
        rotate_ticks: u64,
    },
    /// The zones of `order` start owned by `defenders` and unlock one at a time. When the attackers
    /// take the active zone it stays theirs and locks, and the next unlocks; taking the last ends
    /// the match for the attackers ([`EndReason::ZonesCaptured`]). With a phase time limit the
    /// defenders win at time-up (ADR-8485's clock reads them as the sole leader).
    AttackDefend {
        /// The attacking team.
        attackers: u8,
        /// The defending team.
        defenders: u8,
        /// Zone ids in unlock order.
        order: Vec<u32>,
    },
}

impl ZoneMode {
    /// The mode as a lowercase string (`"domination"`, `"hardpoint"`, `"attack_defend"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Domination => "domination",
            Self::Hardpoint { .. } => "hardpoint",
            Self::AttackDefend { .. } => "attack_defend",
        }
    }
}

/// The zone mode, its score limit and its running state, kept on [`ObjectiveBoard::zone_rules`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ZoneRules {
    /// The mode.
    pub mode: ZoneMode,
    /// Team score that ends the match ([`EndReason::ZoneScoreLimitReached`]); `None` for none.
    pub score_limit: Option<u32>,
    /// Index of the active zone in the hardpoint rotation or the attack/defend order.
    pub active: usize,
    /// Zone ticks since the active hardpoint activated.
    pub since_rotate: u64,
}

impl ZoneRules {
    /// True for the default (domination, no score limit, nothing running): the board does not
    /// serialize it.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl ObjectiveBoard {
    /// Install a capture zone as the host's entity `entity_id` (map data or a test). Any number of
    /// zones may be installed; each counts against the objective cap.
    pub fn install_zone(&mut self, entity_id: u32, settings: ZoneSettings) -> Result<(), RulesError> {
        self.check_room(entity_id)?;
        let center = settings.volume.center();
        self.objectives.push(Objective {
            entity_id,
            kind: ObjectiveKind::Zone,
            team_id: 0,
            carrier_id: None,
            at_base: false,
            base_position: center,
            position: center,
            volume: Some(settings.volume),
            dropped_tick: None,
            zone: Some(Zone::new(settings)),
        });
        Ok(())
    }

    /// The zone with entity id `id`.
    pub fn zone(&self, id: u32) -> Option<&Zone> {
        self.objective(id).and_then(|o| o.zone.as_ref())
    }

    /// Zone ids on the board, ascending.
    pub fn zone_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self
            .objectives
            .iter()
            .filter(|o| o.is_zone() && o.zone.is_some())
            .map(|o| o.entity_id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Set the zone mode and score limit, and lock, unlock and seed owners for it: hardpoint
    /// unlocks the first zone of the rotation and locks the rest of the list; attack/defend gives
    /// every zone of the order to the defenders, unlocks the first and locks the rest. Zones a list
    /// does not name keep their state; the zones attack/defend gives the defenders start with no
    /// progress, hold or decay count. Refused (nothing changes) when a list is empty, names an id
    /// that is not a zone or names an id twice, when `rotate_ticks` is 0, or when the
    /// attack/defend teams are equal or not fighting teams.
    pub fn set_zone_rules(&mut self, mode: ZoneMode, score_limit: Option<u32>) -> Result<(), RulesError> {
        let list: &[u32] = match &mode {
            ZoneMode::Domination => &[],
            ZoneMode::Hardpoint {
                rotation,
                rotate_ticks,
            } => {
                if *rotate_ticks == 0 {
                    return Err(RulesError::ZoneSettings {
                        field: "rotate_ticks",
                    });
                }
                if rotation.is_empty() {
                    return Err(RulesError::ZoneSettings { field: "rotation" });
                }
                rotation
            }
            ZoneMode::AttackDefend {
                attackers,
                defenders,
                order,
            } => {
                if !self.settings.is_fighting_team(*attackers) {
                    return Err(RulesError::ZoneSettings { field: "attackers" });
                }
                if !self.settings.is_fighting_team(*defenders) || attackers == defenders {
                    return Err(RulesError::ZoneSettings { field: "defenders" });
                }
                if order.is_empty() {
                    return Err(RulesError::ZoneSettings { field: "order" });
                }
                order
            }
        };
        if let Some(&id) = list.iter().find(|&&id| self.zone(id).is_none()) {
            return Err(RulesError::ZoneUnknown { id });
        }
        // A repeated id would lock the zone the list says is active (the later copy's lock wins).
        let mut sorted = list.to_vec();
        sorted.sort_unstable();
        if sorted.windows(2).any(|pair| pair[0] == pair[1]) {
            let field = match &mode {
                ZoneMode::Hardpoint { .. } => "rotation",
                _ => "order",
            };
            return Err(RulesError::ZoneSettings { field });
        }
        self.zone_rules = ZoneRules {
            mode,
            score_limit,
            active: 0,
            since_rotate: 0,
        };
        self.start_zone_mode();
        Ok(())
    }

    /// Every zone back to neutral with no progress, and the zone mode restarted (a round start or
    /// the end of a warm-up).
    pub fn reset_zones(&mut self) {
        for zone in self.objectives.iter_mut().filter_map(|o| o.zone.as_mut()) {
            zone.reset();
        }
        self.zone_rules.active = 0;
        self.zone_rules.since_rotate = 0;
        self.start_zone_mode();
    }

    /// The team scores the phase clock reads at time-up: the board's scores, except in
    /// attack/defend, where the defenders are the sole leader (the attackers win only by taking
    /// the last zone).
    pub fn time_up_scores(&self) -> std::borrow::Cow<'_, [u32]> {
        match &self.zone_rules.mode {
            ZoneMode::AttackDefend { defenders, .. } => {
                let mut scores = vec![0; self.team_scores.len()];
                if let Some(score) = scores.get_mut(usize::from(*defenders)) {
                    *score = 1;
                }
                std::borrow::Cow::Owned(scores)
            }
            _ => std::borrow::Cow::Borrowed(&self.team_scores),
        }
    }

    /// The capture-zone step at match tick `tick` (see the module docs). Does nothing once the
    /// match is over or without zones. Units are counted per team, so their order does not matter;
    /// zones run in ascending entity id.
    pub fn zones_tick(&mut self, tick: u64, units: &[Contestant]) {
        if self.match_over {
            return;
        }
        let mut order: Vec<(u32, usize)> = self
            .objectives
            .iter()
            .enumerate()
            .filter(|(_, o)| o.zone.is_some())
            .map(|(index, o)| (o.entity_id, index))
            .collect();
        if order.is_empty() {
            return;
        }
        let _span = tracing::trace_span!("match_zones_tick", zones = order.len()).entered();
        order.sort_unstable();
        self.rotate_hardpoint(tick);
        let mut counts = vec![0u32; usize::from(self.settings.max_teams) + 1];
        let mut inside: Vec<(u32, u8)> = Vec::with_capacity(units.len());
        // Every unlocked zone takes the tick; the first end (lowest entity id) is applied after
        // the whole pass, so a round end the phase machine reopens cannot skip later zones.
        let mut first_end: Option<(u8, EndReason)> = None;
        for (id, index) in order {
            let Some(zone) = self.objectives[index].zone else {
                continue;
            };
            // The zone mode's lock and a host or logic-graph lock (`ObjectiveBoard::set_locked`,
            // ADR-8731) both stop the zone.
            if zone.locked || self.is_locked(id) {
                continue;
            }
            // A unit id the host lists twice counts once (its lowest team when the entries
            // disagree); sorting first keeps the count independent of the host's order.
            inside.clear();
            inside.extend(
                units
                    .iter()
                    .filter(|u| {
                        u.alive
                            && self.settings.is_fighting_team(u.team)
                            && zone.settings.volume.contains(u.position)
                    })
                    .map(|u| (u.id, u.team)),
            );
            inside.sort_unstable();
            inside.dedup_by_key(|(unit, _)| *unit);
            counts.fill(0);
            for &(_, team) in &inside {
                let slot = &mut counts[usize::from(team)];
                *slot = slot.saturating_add(1);
            }
            let presence = zone
                .settings
                .contest
                .resolve(&counts, zone.settings.max_rate_units);
            let mut zone = zone;
            let flipped = zone.step(presence);
            let mut point = false;
            if zone.owner != 0 {
                zone.held_ticks = zone.held_ticks.saturating_add(1);
                if let Some(every) = zone.settings.owner_scores_every {
                    point = zone.held_ticks.is_multiple_of(u64::from(every.max(1)));
                }
            }
            self.objectives[index].zone = Some(zone);
            if let Some(previous) = flipped {
                self.events.push(RulesEvent::ZoneCaptured {
                    tick,
                    zone: id,
                    team: zone.owner,
                    previous,
                });
                if let Some(attackers) = self.advance_attack(tick, id, zone.owner) {
                    first_end.get_or_insert((attackers, EndReason::ZonesCaptured));
                }
            }
            if point {
                let team = zone.owner;
                let score = self.add_point(team);
                self.events.push(RulesEvent::ZonePoint {
                    tick,
                    zone: id,
                    team,
                    score,
                });
                if self
                    .zone_rules
                    .score_limit
                    .is_some_and(|limit| score >= limit.max(1))
                {
                    first_end.get_or_insert((team, EndReason::ZoneScoreLimitReached));
                }
            }
        }
        if let Some((team, reason)) = first_end {
            self.end_match(tick, team, reason);
        }
    }

    /// Apply the mode's locks and owners from `active` on.
    fn start_zone_mode(&mut self) {
        let active = self.zone_rules.active;
        match self.zone_rules.mode.clone() {
            ZoneMode::Domination => {
                for zone in self.objectives.iter_mut().filter_map(|o| o.zone.as_mut()) {
                    zone.locked = zone.settings.locked;
                }
            }
            ZoneMode::Hardpoint { rotation, .. } => {
                for (index, id) in rotation.iter().enumerate() {
                    self.set_zone_lock(*id, index != active);
                }
            }
            ZoneMode::AttackDefend { defenders, order, .. } => {
                for (index, id) in order.iter().enumerate() {
                    if let Some(zone) = self.zone_mut(*id) {
                        if index >= active {
                            // The defenders' zone starts clean: no hold timer, capture bar or
                            // decay count carried over from the previous mode.
                            zone.owner = defenders;
                            zone.capturing_team = 0;
                            zone.progress = 0;
                            zone.held_ticks = 0;
                            zone.empty_ticks = 0;
                        }
                        zone.locked = index != active;
                    }
                }
            }
        }
    }

    fn zone_mut(&mut self, id: u32) -> Option<&mut Zone> {
        self.objectives
            .iter_mut()
            .find(|o| o.entity_id == id)
            .and_then(|o| o.zone.as_mut())
    }

    fn set_zone_lock(&mut self, id: u32, locked: bool) {
        if let Some(zone) = self.zone_mut(id) {
            zone.locked = locked;
        }
    }

    /// Count the hardpoint clock and move to the next zone when it runs out.
    fn rotate_hardpoint(&mut self, tick: u64) {
        let ZoneMode::Hardpoint {
            rotation,
            rotate_ticks,
        } = &self.zone_rules.mode
        else {
            return;
        };
        let len = rotation.len();
        if len == 0 {
            return;
        }
        let period = (*rotate_ticks).max(1);
        let next_index = (self.zone_rules.active + 1) % len;
        let (old, next) = (rotation[self.zone_rules.active % len], rotation[next_index]);
        self.zone_rules.since_rotate = self.zone_rules.since_rotate.saturating_add(1);
        if self.zone_rules.since_rotate <= period {
            return;
        }
        self.zone_rules.active = next_index;
        self.zone_rules.since_rotate = 1;
        if let Some(zone) = self.zone_mut(old) {
            zone.reset();
            zone.locked = true;
        }
        if let Some(zone) = self.zone_mut(next) {
            zone.locked = false;
        }
        self.events.push(RulesEvent::ZoneActivated { tick, zone: next });
    }

    /// Attack/defend: the attackers took zone `id`; lock it and unlock the next. Returns the
    /// attackers when that was the last zone of the order (the caller ends the match).
    fn advance_attack(&mut self, tick: u64, id: u32, team: u8) -> Option<u8> {
        let ZoneMode::AttackDefend { attackers, order, .. } = &self.zone_rules.mode else {
            return None;
        };
        if team != *attackers || order.get(self.zone_rules.active) != Some(&id) {
            return None;
        }
        let attackers = *attackers;
        let next = order.get(self.zone_rules.active + 1).copied();
        self.set_zone_lock(id, true);
        match next {
            Some(next) => {
                self.zone_rules.active += 1;
                self.set_zone_lock(next, false);
                self.events.push(RulesEvent::ZoneActivated { tick, zone: next });
                None
            }
            None => Some(attackers),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contest_rules_resolve_counts() {
        let counts = [0, 3, 1, 0, 0];
        assert_eq!(Contest::AnyEnemyBlocks.resolve(&counts, 8), Presence::Contested);
        assert_eq!(
            Contest::Majority.resolve(&counts, 8),
            Presence::Pushed { team: 1, rate: 3 }
        );
        assert_eq!(
            Contest::Weighted.resolve(&counts, 8),
            Presence::Pushed { team: 1, rate: 2 }
        );
        assert_eq!(
            Contest::Majority.resolve(&[0, 2, 2, 0, 0], 8),
            Presence::Contested
        );
        assert_eq!(
            Contest::Weighted.resolve(&[0, 2, 1, 1, 0], 8),
            Presence::Contested
        );
        assert_eq!(Contest::Weighted.resolve(&[0, 0, 0, 0, 0], 8), Presence::Empty);
        assert_eq!(
            Contest::AnyEnemyBlocks.resolve(&[0, 0, 0, 7, 0], 0),
            Presence::Pushed { team: 3, rate: 1 },
            "a zero cap reads as one"
        );
        // The top team found later in the walk still wins Majority.
        assert_eq!(
            Contest::Majority.resolve(&[0, 1, 4, 2, 0], 8),
            Presence::Pushed { team: 2, rate: 4 }
        );
    }

    #[test]
    fn a_pushing_team_unwinds_another_teams_progress_first() {
        let mut zone = Zone::new(ZoneSettings {
            capture_ticks: 10,
            ..ZoneSettings::new(HillVolume {
                min: [0.0; 3],
                max: [1.0; 3],
            })
        });
        for _ in 0..4 {
            zone.step(Presence::Pushed { team: 1, rate: 1 });
        }
        assert_eq!((zone.capturing_team, zone.progress), (1, 4));
        zone.step(Presence::Pushed { team: 2, rate: 3 });
        assert_eq!((zone.capturing_team, zone.progress), (1, 1));
        zone.step(Presence::Pushed { team: 2, rate: 3 });
        assert_eq!((zone.capturing_team, zone.progress), (0, 0), "no carry-over");
        zone.step(Presence::Pushed { team: 2, rate: 3 });
        assert_eq!((zone.capturing_team, zone.progress), (2, 3));
    }

    #[test]
    fn zone_rules_refuse_bad_lists_and_leave_the_board_unchanged() {
        let mut board = ObjectiveBoard::new(
            super::super::mode::MatchMode::Domination,
            super::super::objective::ObjectiveSettings::default(),
        );
        let volume = HillVolume {
            min: [0.0; 3],
            max: [1.0; 3],
        };
        board.install_zone(5, ZoneSettings::new(volume)).expect("zone");
        board.install_hill_volume(6, volume).expect("hill");
        let before = board.clone();
        assert_eq!(
            board.set_zone_rules(
                ZoneMode::Hardpoint {
                    rotation: vec![5, 6],
                    rotate_ticks: 10
                },
                None
            ),
            Err(RulesError::ZoneUnknown { id: 6 })
        );
        assert_eq!(
            board.set_zone_rules(
                ZoneMode::Hardpoint {
                    rotation: vec![5],
                    rotate_ticks: 0
                },
                None
            ),
            Err(RulesError::ZoneSettings {
                field: "rotate_ticks"
            })
        );
        assert_eq!(
            board.set_zone_rules(
                ZoneMode::AttackDefend {
                    attackers: 1,
                    defenders: 1,
                    order: vec![5]
                },
                None
            ),
            Err(RulesError::ZoneSettings { field: "defenders" })
        );
        assert_eq!(
            board.set_zone_rules(
                ZoneMode::AttackDefend {
                    attackers: 1,
                    defenders: 2,
                    order: vec![]
                },
                None
            ),
            Err(RulesError::ZoneSettings { field: "order" })
        );
        assert_eq!(board, before);
    }
}
