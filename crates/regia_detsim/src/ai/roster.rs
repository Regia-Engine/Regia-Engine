// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/ai/combat.rs, commit c9513623f, tag regia-absorb-4), ADR-8012.
// Mark: adapt (lines 77-144, `TeamTargets::build_for` and `allies`, plan row S-09 re-copy). Kept: the team lists are
// built once per tick and hold each team's rows in world (slot) order, the same list a filter over the whole world
// gives. Named changes: the lists hold slots of any host array, not resolved game targets; a team is a host
// closure's answer (`None` keeps a row off every roster); the lists are one counting-sort table reused across
// ticks instead of a map of vectors; the team cap is a setting (`RosterSettings`).
//
// Portions copied from Direct Order 27 (ir-sim/src/state.rs, commit c9513623f, tag regia-absorb-4), ADR-8012.
// Mark: adapt (lines 2523-2529 and 20932-20993, the entity id -> slot cache, plan row S-09 re-copy). Kept: the index
// is rebuilt once per tick, the first slot of an id wins (what a forward scan returns), a lookup checks that the
// slot still holds the id and falls back to the forward scan when it does not, a row pushed at the end is noted
// without a rebuild. Named changes: a sorted `(id, slot)` table instead of a hash map (no hash containers in
// lockstep code), every `(id, slot)` pair is kept so a reported remove never loses a later duplicate, removes are
// reported (`SlotIndex::remove`), and the lookup counts its fallbacks.

//! [`TeamRosters`] and [`SlotIndex`]: per-tick team lists and an id -> slot index, so AI queries read one
//! team's rows instead of filtering the whole world once per unit.
//!
//! Both index *slots* (positions in the host's row array, the same slots [`crate::spatial::DetSpatial`]
//! uses), never entity ids, and both give exactly the answer of the linear scan they replace:
//!
//! - `rosters.team(t)` is `rows.iter().enumerate().filter(|r| team_of(r) == Some(t))` as slots, ascending.
//! - `rosters.others_into(t, out)` is the same filter for `team_of(r) != Some(t)` (live rows of every other
//!   team), ascending.
//! - `index.slot_of(id)` is `rows.iter().position(|r| id_of(r) == id)`.
//!
//! So an AI layer that switches from the scan to these types keeps its results bit for bit, and its
//! per-tick cost drops from `units x rows` team reads to one read per row.

use std::cell::Cell;

/// Limits of [`TeamRosters`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RosterSettings {
    /// Teams `0..max_teams` are accepted; a row whose team is at or past it fails the build
    /// ([`RosterError::TeamOutOfRange`]). The default, 256, accepts every `u8` team.
    pub max_teams: u16,
}

impl Default for RosterSettings {
    fn default() -> Self {
        RosterSettings { max_teams: 256 }
    }
}

/// Why [`TeamRosters::build`] refused the rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RosterError {
    /// A row's team is at or past [`RosterSettings::max_teams`].
    TeamOutOfRange {
        /// The first row (slot) with such a team.
        slot: usize,
        /// Its team.
        team: u8,
        /// The setting it broke.
        max_teams: u16,
    },
}

impl RosterError {
    /// Stable lowercase code for host agent rows and logs.
    pub fn code(self) -> &'static str {
        match self {
            RosterError::TeamOutOfRange { .. } => "roster_team_out_of_range",
        }
    }
}

impl std::fmt::Display for RosterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RosterError::TeamOutOfRange {
                slot,
                team,
                max_teams,
            } => write!(
                f,
                "roster_team_out_of_range: row {slot} is on team {team}, the rosters accept teams below {max_teams}"
            ),
        }
    }
}

impl std::error::Error for RosterError {}

/// Counters of a [`TeamRosters`] since construction or [`TeamRosters::reset_stats`]. No wall-clock reads;
/// a tick profiler shows them beside its timings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RosterStats {
    /// Successful builds.
    pub builds: u64,
    /// Rows read by builds (one team test per row per build).
    pub rows_read: u64,
    /// Rows on some roster after the last build (a gauge).
    pub members: usize,
    /// Highest listed team plus one after the last build (a gauge).
    pub teams: usize,
}

/// The `group_mask` bit of a team for [`crate::spatial::SpatialRow`]: `1 << team` for teams 0-63, and 0
/// (matches no query) past that.
pub const fn team_group_mask(team: u8) -> u64 {
    if team < 64 { 1u64 << team } else { 0 }
}

/// Marks a row that is on no roster in the per-slot team table.
const NO_TEAM: u16 = u16::MAX;

/// Each team's rows (slots of the host's row array) in ascending slot order, built once per tick.
///
/// [`Self::build`] reads every row once (one call of the host's team closure per row) and lays the teams
/// out as one counting-sort table, so the lists come out in slot order with no sort. After the first
/// builds the table allocates only when the row count or team count grows.
#[derive(Debug, Clone, Default)]
pub struct TeamRosters {
    settings: RosterSettings,
    /// `starts[t]..starts[t + 1]` is team `t`'s range of `slots`.
    starts: Vec<u32>,
    slots: Vec<u32>,
    /// The team of every slot of the last build (`NO_TEAM` when none).
    team_of_slot: Vec<u16>,
    stats: RosterStats,
}

impl TeamRosters {
    /// Empty rosters with these limits.
    pub fn new(settings: RosterSettings) -> Self {
        TeamRosters {
            settings,
            ..TeamRosters::default()
        }
    }

    /// The limits.
    pub fn settings(&self) -> RosterSettings {
        self.settings
    }

    /// The counters.
    pub fn stats(&self) -> RosterStats {
        self.stats
    }

    /// Zero the build counters (the gauges keep the last build's values).
    pub fn reset_stats(&mut self) {
        self.stats.builds = 0;
        self.stats.rows_read = 0;
    }

    /// Rebuild from the host's rows: `team_of(row)` names the roster a row is on (`None`: none, e.g. a dead
    /// unit). Slot `i` is `rows[i]`. On an error the rosters are left empty.
    pub fn build<T>(
        &mut self,
        rows: &[T],
        mut team_of: impl FnMut(&T) -> Option<u8>,
    ) -> Result<(), RosterError> {
        let _span = tracing::trace_span!("regia_detsim::ai::rosters_build", rows = rows.len()).entered();
        self.starts.clear();
        self.slots.clear();
        self.team_of_slot.clear();
        self.stats.members = 0;
        self.stats.teams = 0;
        let mut team_count = 0usize;
        let mut failed = None;
        for (slot, row) in rows.iter().enumerate() {
            let team = team_of(row);
            match team {
                Some(t) if u16::from(t) >= self.settings.max_teams => {
                    failed = Some(RosterError::TeamOutOfRange {
                        slot,
                        team: t,
                        max_teams: self.settings.max_teams,
                    });
                    break;
                }
                Some(t) => {
                    team_count = team_count.max(usize::from(t) + 1);
                    self.team_of_slot.push(u16::from(t));
                }
                None => self.team_of_slot.push(NO_TEAM),
            }
        }
        if let Some(err) = failed {
            self.team_of_slot.clear();
            return Err(err);
        }
        // Counting sort: counts, then running starts, then a stable fill in slot order.
        self.starts.resize(team_count + 1, 0);
        for &t in &self.team_of_slot {
            if t != NO_TEAM {
                self.starts[usize::from(t) + 1] += 1;
            }
        }
        for t in 0..team_count {
            self.starts[t + 1] += self.starts[t];
        }
        let members = self.starts[team_count] as usize;
        self.slots.resize(members, 0);
        // Reuse the counts as write cursors: `starts[t]` advances while team `t` fills, then shifts back.
        for (slot, &t) in self.team_of_slot.iter().enumerate() {
            if t != NO_TEAM {
                let at = &mut self.starts[usize::from(t)];
                self.slots[*at as usize] = slot as u32;
                *at += 1;
            }
        }
        for t in (1..=team_count).rev() {
            self.starts[t] = self.starts[t - 1];
        }
        self.starts[0] = 0;
        self.stats.builds = self.stats.builds.saturating_add(1);
        self.stats.rows_read = self.stats.rows_read.saturating_add(rows.len() as u64);
        self.stats.members = members;
        self.stats.teams = team_count;
        Ok(())
    }

    /// The slots of `team`'s rows, ascending (empty for a team with no rows).
    pub fn team(&self, team: u8) -> &[u32] {
        let t = usize::from(team);
        match (self.starts.get(t), self.starts.get(t + 1)) {
            (Some(&a), Some(&b)) => &self.slots[a as usize..b as usize],
            _ => &[],
        }
    }

    /// Every team with rows, ascending, with its slots.
    pub fn teams(&self) -> impl Iterator<Item = (u8, &[u32])> + '_ {
        (0..self.team_count())
            .map(|t| (t as u8, self.team(t as u8)))
            .filter(|(_, slots)| !slots.is_empty())
    }

    /// Fill `out` with the slots of every listed row that is not on `team` (its enemies in a two-sided
    /// match), ascending. `out` is cleared first; it allocates only when it must grow.
    pub fn others_into(&self, team: u8, out: &mut Vec<u32>) {
        out.clear();
        let own = self.team(team).len();
        out.reserve(self.slots.len().saturating_sub(own));
        let mut other_teams = self.teams().filter(|(t, _)| *t != team);
        match (other_teams.next(), other_teams.next()) {
            (None, _) => {}
            // One other team: its run is already ascending.
            (Some((_, slots)), None) => out.extend_from_slice(slots),
            // Several: one pass over the per-slot team table yields them ascending with no sort.
            _ => {
                let own = u16::from(team);
                out.extend(
                    self.team_of_slot
                        .iter()
                        .enumerate()
                        .filter(|&(_, &t)| t != NO_TEAM && t != own)
                        .map(|(slot, _)| slot as u32),
                );
            }
        }
    }

    /// The team the last build listed `slot` under (`None`: on no roster, or past the rows).
    pub fn team_of_slot(&self, slot: usize) -> Option<u8> {
        match self.team_of_slot.get(slot) {
            Some(&t) if t != NO_TEAM => Some(t as u8),
            _ => None,
        }
    }

    /// Highest listed team plus one (0 when no row is listed).
    pub fn team_count(&self) -> usize {
        self.starts.len().saturating_sub(1)
    }

    /// Rows on some roster.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// No row is on any roster.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

/// Counters of a [`SlotIndex`] since construction or [`SlotIndex::reset_stats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SlotIndexStats {
    /// Builds.
    pub builds: u64,
    /// [`SlotIndex::find`] calls answered from the index.
    pub hits: u64,
    /// [`SlotIndex::find`] calls whose indexed slot no longer held the id (a change nobody reported) and that
    /// the forward scan answered instead. Zero when every push and remove is reported.
    pub scan_fallbacks: u64,
}

/// Entity id -> first slot of the host's row array, answering what a forward scan answers.
///
/// Build it once per tick ([`Self::build`]); report a row pushed at the end ([`Self::push`]) or removed
/// with a shift ([`Self::remove`]) to keep it exact between builds. [`Self::find`] checks that the slot it
/// returns still holds the id and falls back to the forward scan when it does not, so a change you forgot to
/// report costs a scan (counted in [`SlotIndexStats::scan_fallbacks`]) and never returns another row. With
/// duplicate ids, the first duplicate is kept exact only while every change is reported.
///
/// The table is a sorted `(id, slot)` list: lookups are a binary search, and a build allocates only when
/// the row count grows.
#[derive(Debug, Clone, Default)]
pub struct SlotIndex {
    /// Every `(id, slot)` pair, sorted by id then slot.
    entries: Vec<(u32, u32)>,
    stats: Cell<SlotIndexStats>,
}

impl SlotIndex {
    /// Rebuild from the host's rows: slot `i` is `rows[i]`, its id `id_of(&rows[i])`.
    pub fn build<T>(&mut self, rows: &[T], mut id_of: impl FnMut(&T) -> u32) {
        let _span = tracing::trace_span!("regia_detsim::ai::slot_index_build", rows = rows.len()).entered();
        self.entries.clear();
        self.entries.extend(
            rows.iter()
                .enumerate()
                .map(|(slot, row)| (id_of(row), slot as u32)),
        );
        // Pairs are unique (slots are), so the unstable sort has one result.
        self.entries.sort_unstable();
        let mut stats = self.stats.get();
        stats.builds = stats.builds.saturating_add(1);
        self.stats.set(stats);
    }

    /// Forget every entry; [`Self::find`] answers by the forward scan until the next build.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// The first indexed slot of `id`, without checking the rows.
    pub fn slot_of(&self, id: u32) -> Option<usize> {
        let at = self.entries.partition_point(|&(e, _)| e < id);
        match self.entries.get(at) {
            Some(&(e, slot)) if e == id => Some(slot as usize),
            _ => None,
        }
    }

    /// The first slot of `rows` whose id is `id`: the indexed slot when the row there still has the id,
    /// otherwise the forward scan's answer.
    pub fn find<T>(&self, rows: &[T], id: u32, mut id_of: impl FnMut(&T) -> u32) -> Option<usize> {
        let mut stats = self.stats.get();
        let hit = self
            .slot_of(id)
            .filter(|&slot| rows.get(slot).is_some_and(|row| id_of(row) == id));
        let found = match hit {
            Some(slot) => {
                stats.hits = stats.hits.saturating_add(1);
                Some(slot)
            }
            None => {
                let scanned = rows.iter().position(|row| id_of(row) == id);
                // The index was wrong (a stale slot, or a row it never heard of): count the scan.
                if scanned.is_some() || self.slot_of(id).is_some() {
                    stats.scan_fallbacks = stats.scan_fallbacks.saturating_add(1);
                }
                scanned
            }
        };
        self.stats.set(stats);
        found
    }

    /// Note a row with `id` pushed at the end of the rows. Its slot is [`Self::len`]: the index must hold
    /// every row (built from them, with every change since reported).
    pub fn push(&mut self, id: u32) {
        let slot = self.entries.len() as u32;
        let at = self.entries.partition_point(|&e| e <= (id, slot));
        self.entries.insert(at, (id, slot));
    }

    /// Note that the row in `slot` was removed and the rows after it moved down by one (`Vec::remove`).
    pub fn remove(&mut self, slot: usize) {
        let slot = slot as u32;
        self.entries.retain(|&(_, s)| s != slot);
        // Every later slot moves down by one; the (id, slot) order is unchanged by a uniform shift.
        for entry in &mut self.entries {
            if entry.1 > slot {
                entry.1 -= 1;
            }
        }
    }

    /// Indexed rows.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// No row is indexed.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The counters.
    pub fn stats(&self) -> SlotIndexStats {
        self.stats.get()
    }

    /// Zero the counters.
    pub fn reset_stats(&mut self) {
        self.stats.set(SlotIndexStats::default());
    }
}
