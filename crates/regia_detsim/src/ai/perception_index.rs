// SPDX-License-Identifier: MPL-2.0
//! Perception on the spatial index (gap AI-4, ADR-8518): the sighting refresh without all-pairs scans.
//!
//! Original code. Range pruning is a uniform-grid range query (Ericson, *Real-Time Collision
//! Detection*, § 7.1) through [`DetSpatial`]; the board rules are the linear refresh's
//! ([`SightingsBoard::refresh_linear`]) unchanged.
//!
//! The linear refresh walks, per enemy (ascending entity id) and per observing team (ascending), every
//! ally of that team in observer-list order: first the line-of-sight loop (allies whose re-check phase
//! is due, within their sight range, the first clear line wins), then, when no ally saw it, the radar
//! loop (any ally within the board's radar range times its `radar_scale`). The indexed refresh asks the
//! same questions of fewer allies:
//!
//! - Each refresh builds one [`DetSpatial`] over the offered enemies and one per observing team over
//!   its observers. Slots are list positions (a team's observers compacted in observer-list order), so
//!   an index hands candidates out in list order and a team query never reads another team's rows.
//! - Line of sight: one query per due observer (its re-check phase is due) around the observer with its
//!   own sight range, keeping the (enemy, observer) pairs the linear loop's own test accepts
//!   (`dist_xz(ally, enemy) <= ally.sight_range_m`, the enemy on another team). Sorted by enemy, then
//!   observer, they are the in-range due allies of each enemy in the linear loop's order; the line of
//!   sight is then asked per enemy and team in that order and stops at the first clear line, so it is
//!   asked the same pairs in the same order.
//! - Radar: when no line was clear, the loop only needs whether some ally passes the linear test, in any
//!   order. A radius-0 query around the enemy (its own cell) is tried first, then one query with the
//!   team's largest radar reach for the enemy's lit state; each stops at the first ally the linear test
//!   accepts.
//! - Memory, decay and forgetting are the linear refresh's, with sorted vectors in place of the per-call
//!   ordered maps, so a warm [`PerceptionIndex`] allocates nothing.
//!
//! Boards with more than [`PERCEPTION_INDEX_MAX_TEAMS`] teams (past the `u8` team ids) run the linear refresh.

use std::ops::ControlFlow;

use super::los::LineOfSight;
use super::perception::{
    KnownAt, PerceptEnemy, PerceptUnit, PerceptionParams, SightSource, SightingsBoard, dist_xz,
};
use crate::broadphase::{BroadBody, BroadphaseConfig, FloorRule};
use crate::float::det_max;
use crate::spatial::{DetSpatial, SpatialRow};

/// Most observing teams the indexed path serves (the board's team ids are `u8`); larger boards run the scan.
pub const PERCEPTION_INDEX_MAX_TEAMS: usize = 256;

/// Grid settings of a [`PerceptionIndex`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PerceptionIndexSettings {
    /// Grid cell edge (metres). Any positive finite value gives the same boards; it only moves the cost.
    /// About the radar range is a good start (each team's index holds only that team's observers): the
    /// default 384 m suits a 400 m radar.
    pub cell_size_m: f32,
    /// Most grid cells per team index (memory and build cost: every build resets this many cell heads);
    /// the cell edge grows when a team's observers spread wider. The default 4096 covers 24 km square at
    /// 384 m cells.
    pub max_cells: usize,
}

impl Default for PerceptionIndexSettings {
    fn default() -> Self {
        PerceptionIndexSettings {
            cell_size_m: 384.0,
            max_cells: 4096,
        }
    }
}

/// Counters of a [`PerceptionIndex`] since construction or [`PerceptionIndex::reset_stats`]. No
/// wall-clock reads; a tick profiler shows them beside its timings.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PerceptionIndexStats {
    /// Refreshes run through this index.
    pub refreshes: u64,
    /// Refreshes that ran the linear scan instead (more than [`PERCEPTION_INDEX_MAX_TEAMS`] teams).
    pub linear_fallbacks: u64,
    /// Spatial queries (one per due observer, and one or two radar queries per enemy and team no line saw).
    pub queries: u64,
    /// Allies the queries handed to the exact distance test.
    pub candidates: u64,
    /// Line-of-sight questions asked ([`LineOfSight::clear`] calls).
    pub los_calls: u64,
    /// Allies the linear scan's line-of-sight loop would have read: the sum over enemies and observing
    /// teams of the team's size. `candidates / scan_pairs` is the work the index saved.
    pub scan_pairs: u64,
}

/// One observing team's radar index over its observers.
#[derive(Debug, Clone)]
struct TeamIndex {
    all: DetSpatial,
    all_rows: Vec<SpatialRow>,
    /// Compact slot -> observer index, ascending (so slot order is observer-list order).
    all_units: Vec<u32>,
    /// Some observer's re-check phase is due this tick.
    rechecked: bool,
    /// The largest radar reach against a dark / a lit enemy.
    radar_dark: f32,
    radar_lit: f32,
}

impl TeamIndex {
    fn new(config: BroadphaseConfig) -> Self {
        TeamIndex {
            all: DetSpatial::new(config),
            all_rows: Vec::new(),
            all_units: Vec::new(),
            rechecked: false,
            radar_dark: f32::NEG_INFINITY,
            radar_lit: f32::NEG_INFINITY,
        }
    }

    fn clear(&mut self) {
        self.all_rows.clear();
        self.all_units.clear();
        self.rechecked = false;
        self.radar_dark = f32::NEG_INFINITY;
        self.radar_lit = f32::NEG_INFINITY;
    }
}

/// Reusable buffers of the indexed sighting refresh ([`SightingsBoard::refresh_with`]).
///
/// Keep one per board and pass it every tick: after the first refreshes it allocates only when the
/// observer count, the enemy count, the in-sight pair count, the team count or a team's board grows.
#[derive(Debug, Clone)]
pub struct PerceptionIndex {
    settings: PerceptionIndexSettings,
    config: BroadphaseConfig,
    teams: Vec<TeamIndex>,
    /// The offered enemies (slot = position in the enemy list).
    enemy_index: DetSpatial,
    enemy_rows: Vec<SpatialRow>,
    /// Observers whose re-check phase is due, ascending.
    due: Vec<u32>,
    /// `enemy << 32 | observer` for every due observer within sight of an enemy of another team, sorted.
    pairs: Vec<u64>,
    /// Per team: `(entity id, first board row)`, sorted by id.
    known_at: Vec<SortedKnownAt>,
    /// Offered enemy ids, sorted.
    offered: Vec<u32>,
    stats: PerceptionIndexStats,
}

impl Default for PerceptionIndex {
    fn default() -> Self {
        Self::new(PerceptionIndexSettings::default())
    }
}

impl PerceptionIndex {
    /// Empty buffers with these grid settings.
    pub fn new(settings: PerceptionIndexSettings) -> Self {
        PerceptionIndex {
            settings,
            config: BroadphaseConfig {
                cell_size: settings.cell_size_m,
                margin: 0.0,
                max_cells: settings.max_cells,
                floor_rule: FloorRule::Never,
                ..BroadphaseConfig::default()
            },
            teams: Vec::new(),
            enemy_index: DetSpatial::new(BroadphaseConfig {
                cell_size: settings.cell_size_m,
                margin: 0.0,
                max_cells: settings.max_cells,
                floor_rule: FloorRule::Never,
                ..BroadphaseConfig::default()
            }),
            enemy_rows: Vec::new(),
            due: Vec::new(),
            pairs: Vec::new(),
            known_at: Vec::new(),
            offered: Vec::new(),
            stats: PerceptionIndexStats::default(),
        }
    }

    /// The grid settings.
    pub fn settings(&self) -> PerceptionIndexSettings {
        self.settings
    }

    /// The counters.
    pub fn stats(&self) -> PerceptionIndexStats {
        self.stats
    }

    /// Zero the counters.
    pub fn reset_stats(&mut self) {
        self.stats = PerceptionIndexStats::default();
    }
}

/// `(entity id, first row)` sorted by id: the ordered-map index of the linear refresh, in a reused vector.
#[derive(Debug, Clone, Default)]
struct SortedKnownAt(Vec<(u32, usize)>);

impl SortedKnownAt {
    /// Rebuild from a board row: the first row of every id.
    fn rebuild(&mut self, ids: impl Iterator<Item = u32>) {
        self.0.clear();
        self.0.extend(ids.enumerate().map(|(at, id)| (id, at)));
        // Sorting by (id, row) and keeping the first of each id keeps the lowest row: the map's
        // `entry(id).or_insert(row)` in row order.
        self.0.sort_unstable();
        self.0.dedup_by_key(|entry| entry.0);
    }
}

impl KnownAt for SortedKnownAt {
    fn get(&self, id: u32) -> Option<usize> {
        self.0
            .binary_search_by_key(&id, |entry| entry.0)
            .ok()
            .map(|i| self.0[i].1)
    }
    fn insert(&mut self, id: u32, at: usize) {
        match self.0.binary_search_by_key(&id, |entry| entry.0) {
            Ok(i) => self.0[i].1 = at,
            Err(i) => self.0.insert(i, (id, at)),
        }
    }
}

fn point(pos: [f32; 3]) -> BroadBody {
    BroadBody::new(pos, 0.0, [0.0, 0.0])
}

impl SightingsBoard {
    /// [`Self::refresh`] with reused buffers: the sighting refresh on the spatial index.
    ///
    /// Same arguments, rules and board as [`Self::refresh_linear`], bit for bit, and the line of sight is
    /// asked the same pairs in the same order; only pairs out of range are never read. Keep one
    /// `scratch` per board across ticks. See [`PerceptionIndex`] for the counters.
    pub fn refresh_with<L: LineOfSight>(
        &mut self,
        scratch: &mut PerceptionIndex,
        los: L,
        tick: u64,
        units: &[PerceptUnit],
        enemies: &[PerceptEnemy],
        params: &[PerceptionParams],
    ) {
        let team_count = self.by_team.len();
        scratch.stats.refreshes += 1;
        if team_count > PERCEPTION_INDEX_MAX_TEAMS {
            scratch.stats.linear_fallbacks += 1;
            self.refresh_linear(los, tick, units, enemies, params);
            return;
        }
        let _span = tracing::trace_span!("det_ai.perception_refresh", units = units.len()).entered();
        let PerceptionIndex {
            config,
            teams,
            enemy_index,
            enemy_rows,
            due,
            pairs,
            known_at,
            offered,
            stats,
            ..
        } = scratch;

        // Each observing team's rows and the due observers, one pass over the observers, in list order.
        let config = *config;
        teams.truncate(team_count);
        teams.resize_with(team_count, || TeamIndex::new(config));
        for team in teams.iter_mut() {
            team.clear();
        }
        due.clear();
        let range_dark = self.detection_range(false);
        let range_lit = self.detection_range(true);
        for (slot, unit) in units.iter().enumerate() {
            let team = usize::from(unit.team);
            let Some(index) = teams.get_mut(team) else {
                continue;
            };
            let interval = params
                .get(team)
                .map(|p| p.los_recheck_interval_ticks)
                .unwrap_or(1);
            index.all_rows.push(SpatialRow {
                slot: index.all_units.len(),
                body: point(unit.pos),
                group_mask: 1,
                flags: 0,
            });
            index.all_units.push(slot as u32);
            index.radar_dark = det_max(index.radar_dark, range_dark * unit.radar_scale);
            index.radar_lit = det_max(index.radar_lit, range_lit * unit.radar_scale);
            if unit.los_recheck_needed(tick, interval) {
                index.rechecked = true;
                due.push(slot as u32);
            }
        }
        enemy_rows.clear();
        enemy_rows.extend(enemies.iter().enumerate().map(|(slot, enemy)| SpatialRow {
            slot,
            body: point(enemy.pos),
            group_mask: 1,
            flags: 0,
        }));
        // Slots are a permutation by construction (0..n in order), so the builds cannot fail; a failure
        // would leave the old rows, so fall back to the scan rather than read them.
        let mut built = enemy_index.build(enemy_rows).is_ok();
        for index in teams.iter_mut() {
            built &= index.all.build(&index.all_rows).is_ok();
        }
        if !built {
            stats.linear_fallbacks += 1;
            self.refresh_linear(los, tick, units, enemies, params);
            return;
        }

        // Direct sight, gathered per due observer: every (enemy, observer) pair the scan's sight test
        // accepts (`dist_xz(ally, enemy) <= ally.sight_range_m`, enemy on another team), sorted by enemy
        // then observer: per enemy, the scan's visiting order.
        pairs.clear();
        for &slot in due.iter() {
            let ally = &units[slot as usize];
            stats.queries += 1;
            let _ = enemy_index.within(point(ally.pos), ally.sight_range_m, 1, |at| {
                stats.candidates += 1;
                let enemy = &enemies[at];
                if enemy.team != ally.team && dist_xz(ally.pos, enemy.pos) <= ally.sight_range_m {
                    pairs.push((at as u64) << 32 | u64::from(slot));
                }
                ControlFlow::Continue(())
            });
        }
        pairs.sort_unstable();

        // Each board's id index, rebuilt once (the linear refresh's per-call ordered maps).
        known_at.resize_with(team_count, SortedKnownAt::default);
        for (index, board) in known_at.iter_mut().zip(&self.by_team) {
            index.rebuild(board.iter().map(|known| known.entity));
        }

        let mut cursor = 0usize;
        for (at, enemy) in enemies.iter().enumerate() {
            let center = point(enemy.pos);
            let first = cursor;
            while cursor < pairs.len() && (pairs[cursor] >> 32) as usize == at {
                cursor += 1;
            }
            let in_sight = &pairs[first..cursor];
            for (team, index) in teams.iter_mut().enumerate() {
                let team_u8 = team as u8;
                if enemy.team == team_u8 {
                    continue;
                }
                stats.scan_pairs += index.all_units.len() as u64;
                let mut seen = false;
                let mut source = SightSource::Radar;
                // Direct LOS: the first due ally within its sight range with a clear line, in list order.
                for &pair in in_sight {
                    let ally = &units[(pair & u64::from(u32::MAX)) as usize];
                    if ally.team == team_u8 {
                        stats.los_calls += 1;
                        if los.clear(ally.pos, enemy.pos) {
                            seen = true;
                            source = SightSource::DirectLos;
                            break;
                        }
                    }
                }
                // Radar contact (team-shared): any ally within its own radar reach. The scan only asks
                // whether one exists, so the allies filed in the enemy's own cell are tried first (a
                // radius-0 query gathers one cell) and the full reach is queried only when none of them
                // passes the scan's test; both use that exact test, so the answer is the scan's.
                if !seen && !index.all_units.is_empty() {
                    let (range, reach) = if enemy.lit {
                        (range_lit, index.radar_lit)
                    } else {
                        (range_dark, index.radar_dark)
                    };
                    let all_units = &index.all_units;
                    let mut radar = |compact: usize| {
                        stats.candidates += 1;
                        let ally = &units[all_units[compact] as usize];
                        if dist_xz(ally.pos, enemy.pos) <= range * ally.radar_scale {
                            ControlFlow::Break(())
                        } else {
                            ControlFlow::Continue(())
                        }
                    };
                    stats.queries += 1;
                    seen = index.all.within(center, 0.0, 1, &mut radar).is_break();
                    if !seen {
                        stats.queries += 1;
                        seen = index.all.within(center, reach, 1, &mut radar).is_break();
                    }
                    // `source` is already `Radar`.
                }
                self.upsert(team_u8, enemy, tick, seen, source, &mut known_at[team]);
            }
        }

        // An id is "in the world" when any offered enemy carries it.
        offered.clear();
        offered.extend(enemies.iter().map(|e| e.id));
        offered.sort_unstable();
        // Decay and forget exactly as the linear refresh does.
        for (team, board) in self.by_team.iter_mut().enumerate() {
            let (decay, forget) = params
                .get(team)
                .map(|p| (p.confidence_decay, p.forget_confidence))
                .unwrap_or((0.96, 0.35));
            let rechecked = teams.get(team).is_some_and(|index| index.rechecked);
            board.retain_mut(|known| {
                if known.sighting.last_seen_tick != tick {
                    let enemy_gone = offered.binary_search(&known.entity).is_err();
                    if enemy_gone || rechecked {
                        known.sighting.confidence *= decay;
                    }
                }
                known.sighting.confidence > forget
            });
        }
    }
}
