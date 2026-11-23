// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/ai/perception.rs lines 1-345 and 386-514, commit c9513623f, tag
// regia-absorb-4; ir-sim/src/ai/difficulty.rs lines 46-60 and 186-195; ir-sim/src/tunnel_light.rs lines 66-82),
// ADR-8012. Mark: adapt (plan row S-10, ADR-8395). The board is DO27's code apart from rustfmt, doc comments and
// these named changes:
// - line of sight goes through the `LineOfSight` trait (`los: L` where DO27 took `Option<&TerrainSampler>`);
// - `PerceptionParams` moved here from DO27's difficulty profiles (plan row S-11 imports it), with DO27's Medium
//   numbers as `Default`;
// - the lit-contact radar bonus (DO27's 150 m headlight constant in its light module) is the board setting
//   `lit_bonus_m`, default 150 m; it is not serialized while it has its default, so the board's serde stream (and
//   a desync hash over it) is DO27's;
// - DO27's weapon stem tables `weapon_dps` / `weapon_band` (game content) are dropped; the tests inline the
//   default row's numbers, and DO27's `weapon_bands_match_the_arsenal_behaviour` stays in DO27;
// - `crate::math::sqrt` is libm 0.2.16 (DO27's bits); a trace span on `refresh`; the first loop of `refresh`
//   walks `rechecked_any.iter_mut().enumerate()` instead of indexing it (clippy `needless_range_loop`, same order);
// - DO27's game names (a unit ability, design-doc sections) are dropped from the comments.
// Plan row S-10 follow-up (gap AI-4, ADR-8518; original code, not DO27's): DO27's scan is kept as
// `refresh_linear`; `refresh` now runs the indexed `refresh_with` (perception_index.rs); `upsert` takes its
// id index through the `KnownAt` trait; `dist_xz` is shared with the indexed path.

//! Perception: the bounded, team-shared picture.
//!
//! A unit knows only what a player could: direct line of sight within a per-class sight range, or radar
//! contact within the team radar range (radar is shared with the team). Sightings decay in confidence while
//! unseen and are dropped below the observing team's forget threshold. No wall-hacks: line of sight is the
//! host's [`LineOfSight`].

use serde::{Deserialize, Serialize};

use super::los::LineOfSight;

/// Base radar range in metres (the default for [`SightingsBoard::new`]).
pub const RADAR_RANGE_M: f32 = 400.0;

/// Extra radar range against a contact that counts as lit (lamps, a flare, a ping): the default
/// [`SightingsBoard::lit_bonus_m`].
pub const LIT_DETECT_BONUS_M: f32 = 150.0;

/// Perception (awareness) parameters of one observing team. Awareness is *not* extra information:
/// a sharper profile trusts its sightings longer and re-checks LOS more often, but it never sees
/// through a wall.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PerceptionParams {
    /// How long a sighting is trusted without refresh before it is stale.
    pub sighting_trust_ticks: u32,
    /// Per-tick confidence multiplier while a sighting is unseen.
    pub confidence_decay: f32,
    /// Ticks between a unit's LOS re-checks against the world.
    pub los_recheck_interval_ticks: u32,
    /// A sighting below this confidence is dropped (forgotten).
    pub forget_confidence: f32,
}

impl Default for PerceptionParams {
    /// A medium profile: trust 60 ticks, decay 0.96 per unseen re-check, a LOS re-check every 12 ticks,
    /// forget below 0.35.
    fn default() -> Self {
        PerceptionParams {
            sighting_trust_ticks: 60,
            confidence_decay: 0.96,
            los_recheck_interval_ticks: 12,
            forget_confidence: 0.35,
        }
    }
}

/// How a sighting was last made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SightSource {
    /// An observer had a clear line of sight within its sight range.
    DirectLos,
    /// Radar contact within the team radar range.
    Radar,
    /// Shared by the team (reserved; the board marks team sharing by the row it writes).
    TeamShared,
}

/// One sighting of one enemy.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Sighting {
    /// The enemy's entity id.
    pub entity: u32,
    /// Where it was last seen.
    pub last_pos: [f32; 3],
    /// Its plan velocity (x, z) when last seen.
    pub last_vel: [f32; 2],
    /// The tick it was last seen.
    pub last_seen_tick: u64,
    /// The tick the sighting was first created. Stable across refreshes; a fire gate uses it for a
    /// reaction delay (a unit may not fire until it has reacted to a newly-acquired target).
    #[serde(default)]
    pub first_seen_tick: u64,
    /// 1 when seen this tick, decaying while unseen.
    pub confidence: f32,
    /// How it was last seen.
    pub source: SightSource,
}

/// Folded threat of one enemy.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ThreatEstimate {
    /// Its damage per second.
    pub dps: f32,
    /// Its weapon's `[min, max]` range band (metres).
    pub range_band: [f32; 2],
    /// It can reach us.
    pub can_reach_me: bool,
    /// It has line of sight to us (the sighting was direct).
    pub incoming: bool,
}

/// A known enemy plus its sighting and threat estimate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct KnownEnemy {
    /// The enemy's entity id.
    pub entity: u32,
    /// The sighting.
    pub sighting: Sighting,
    /// The threat estimate.
    pub threat: ThreatEstimate,
}

fn default_lit_bonus_m() -> f32 {
    LIT_DETECT_BONUS_M
}

fn lit_bonus_is_default(bonus: &f32) -> bool {
    bonus.to_bits() == LIT_DETECT_BONUS_M.to_bits()
}

/// Per-team sightings board. Canonical iteration is sorted by entity id;
/// no hash-map iteration anywhere in the read path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SightingsBoard {
    /// One row per observing team (index = team id), in first-sighting order.
    pub by_team: Vec<Vec<KnownEnemy>>,
    /// The team radar range (metres).
    pub radar_range: f32,
    /// Extra radar range against a lit contact (metres; default [`LIT_DETECT_BONUS_M`]). Serialized only
    /// when it is not the default.
    #[serde(default = "default_lit_bonus_m", skip_serializing_if = "lit_bonus_is_default")]
    pub lit_bonus_m: f32,
}

impl SightingsBoard {
    /// An empty board for `team_count` teams with the given radar range.
    pub fn new(team_count: usize, radar_range: f32) -> Self {
        SightingsBoard {
            by_team: vec![Vec::new(); team_count],
            radar_range,
            lit_bonus_m: LIT_DETECT_BONUS_M,
        }
    }

    /// The same board with another lit-contact radar bonus.
    pub fn with_lit_bonus_m(mut self, lit_bonus_m: f32) -> Self {
        self.lit_bonus_m = lit_bonus_m;
        self
    }

    /// The board row for an observing team.
    pub fn for_team(&self, team: u8) -> &[KnownEnemy] {
        self.by_team
            .get(team as usize)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Radar range against one enemy: the board range, plus the lit bonus
    /// while it counts as lit (light as a tactic).
    pub fn detection_range(&self, enemy_lit: bool) -> f32 {
        if enemy_lit {
            self.radar_range + self.lit_bonus_m
        } else {
            self.radar_range
        }
    }

    /// Rebuild / patch the board in entity-id order.
    ///
    /// `los` is the world's line of sight ([`LineOfSight`]; pass an `Option` of it, `None` being open
    /// field). `enemies` must be sorted by entity id (the caller's canonical order); `units` is the
    /// observer list. `params` is indexed by team (the observing team's perception profile): each team
    /// decays its sightings by its own `confidence_decay` and drops them below its own
    /// `forget_confidence`, and each observer re-checks LOS on its own team's cadence. A team with no
    /// entry uses a LOS re-check every tick and decay 0.96, forget 0.35.
    ///
    /// This runs the indexed refresh ([`Self::refresh_with`]) over a fresh [`PerceptionIndex`]; a host
    /// that refreshes every tick keeps one `PerceptionIndex` and calls `refresh_with` so the buffers are
    /// reused. The board, and the line-of-sight calls in their order, equal [`Self::refresh_linear`]'s.
    ///
    /// [`PerceptionIndex`]: super::PerceptionIndex
    pub fn refresh<L: LineOfSight>(
        &mut self,
        los: L,
        tick: u64,
        units: &[PerceptUnit],
        enemies: &[PerceptEnemy],
        params: &[PerceptionParams],
    ) {
        let mut scratch = super::PerceptionIndex::default();
        self.refresh_with(&mut scratch, los, tick, units, enemies, params);
    }

    /// The reference refresh: every enemy against every ally of every other team (the all-pairs scan the
    /// indexed [`Self::refresh`] replaces). Same arguments and the same board, bit for bit; kept as the
    /// oracle the index is tested against and for hosts that want the scan's cost profile.
    pub fn refresh_linear<L: LineOfSight>(
        &mut self,
        los: L,
        tick: u64,
        units: &[PerceptUnit],
        enemies: &[PerceptEnemy],
        params: &[PerceptionParams],
    ) {
        let _span = tracing::trace_span!("det_ai.perception_refresh_linear").entered();
        // Did each team actually re-evaluate LOS this tick (any ally's re-check
        // phase landed)? The raycast is time-sliced per unit; the *memory* is
        // not. On a tick where no ally re-checks, a team must not treat a
        // not-yet-refreshed sighting as "unseen": it simply has not looked yet.
        // This is what kept an easy bot from ever finishing its reaction delay
        // on terrain: the LOS re-check runs every `los_recheck_interval_ticks`,
        // but the confidence decay used to run every tick, so a continuously
        // visible enemy beyond radar range was "forgotten" between re-checks and
        // its `first_seen_tick` reset.
        let mut rechecked_any: Vec<bool> = vec![false; self.by_team.len()];
        for (team, rechecked) in rechecked_any.iter_mut().enumerate() {
            let team_u8 = team as u8;
            let los_recheck_ticks = params
                .get(team)
                .map(|p| p.los_recheck_interval_ticks)
                .unwrap_or(1);
            *rechecked = units
                .iter()
                .any(|u| u.team == team_u8 && u.los_recheck_needed(tick, los_recheck_ticks));
        }
        // Each team's observers and each board's id index are built once, not once per (enemy, team).
        // Same lists, same order, same first-match rows as the scans they replace.
        let allies_by_team: Vec<Vec<&PerceptUnit>> = (0..self.by_team.len())
            .map(|team| units.iter().filter(|u| u.team == team as u8).collect())
            .collect();
        let mut known_at: Vec<std::collections::BTreeMap<u32, usize>> = self
            .by_team
            .iter()
            .map(|board| {
                let mut index = std::collections::BTreeMap::new();
                for (at, known) in board.iter().enumerate() {
                    index.entry(known.entity).or_insert(at);
                }
                index
            })
            .collect();
        for enemy in enemies {
            for team in 0..self.by_team.len() {
                let team_u8 = team as u8;
                if enemy.team == team_u8 {
                    continue;
                }
                let los_recheck_ticks = params
                    .get(team)
                    .map(|p| p.los_recheck_interval_ticks)
                    .unwrap_or(1);
                let allies = &allies_by_team[team];
                let mut seen = false;
                let mut source = SightSource::Radar;
                // Direct LOS: any ally within its sight range and a clear line.
                for ally in allies {
                    if ally.los_recheck_needed(tick, los_recheck_ticks) {
                        let d = dist_xz(ally.pos, enemy.pos);
                        if d <= ally.sight_range_m && los.clear(ally.pos, enemy.pos) {
                            seen = true;
                            source = SightSource::DirectLos;
                            break;
                        }
                    }
                }
                // Radar contact (team-shared): any ally within its own
                // radar range. Damaged sensors shorten that observer's
                // reach; a lit enemy shows further out. Direct LOS above
                // is unchanged.
                if !seen {
                    let range = self.detection_range(enemy.lit);
                    for ally in allies {
                        if dist_xz(ally.pos, enemy.pos) <= range * ally.radar_scale {
                            seen = true;
                            source = SightSource::Radar;
                            break;
                        }
                    }
                }
                self.upsert(team_u8, enemy, tick, seen, source, &mut known_at[team]);
            }
        }
        // An id is "in the world" when any offered enemy carries it.
        let offered: std::collections::BTreeSet<u32> = enemies.iter().map(|e| e.id).collect();
        // Decay every entry this team has positively failed to re-acquire this
        // tick, and drop the forgotten, per team's own decay. An entry whose
        // enemy is no longer in the world decays every tick; otherwise it only
        // decays on a tick where the team actually re-checked LOS (and came up
        // empty); see the `rechecked_any` note above.
        for (team, board) in self.by_team.iter_mut().enumerate() {
            let (decay, forget) = params
                .get(team)
                .map(|p| (p.confidence_decay, p.forget_confidence))
                .unwrap_or((0.96, 0.35));
            let rechecked = rechecked_any[team];
            board.retain_mut(|known| {
                if known.sighting.last_seen_tick != tick {
                    let enemy_gone = !offered.contains(&known.entity);
                    if enemy_gone || rechecked {
                        known.sighting.confidence *= decay;
                    }
                }
                known.sighting.confidence > forget
            });
        }
    }

    /// Reveal contacts a light or sensor pass grants (lamps, heat, lidar sweeps).
    /// Same-team contacts are skipped; the rest read as seen this tick.
    pub fn reveal_lit(&mut self, team: u8, tick: u64, contacts: &[LitContact]) {
        let mut known_at = std::collections::BTreeMap::new();
        if let Some(board) = self.by_team.get(team as usize) {
            for (at, known) in board.iter().enumerate() {
                known_at.entry(known.entity).or_insert(at);
            }
        }
        for contact in contacts {
            if contact.team == team {
                continue;
            }
            let enemy = PerceptEnemy {
                id: contact.id,
                team: contact.team,
                pos: contact.pos,
                vel: [0.0, 0.0],
                dps: contact.dps,
                band: contact.band,
                lit: true,
            };
            self.upsert(team, &enemy, tick, true, SightSource::DirectLos, &mut known_at);
        }
    }

    /// `known_at` maps an entity id to its first row on this team's board (kept current here).
    pub(super) fn upsert(
        &mut self,
        team: u8,
        enemy: &PerceptEnemy,
        tick: u64,
        seen: bool,
        source: SightSource,
        known_at: &mut impl KnownAt,
    ) {
        let Some(board) = self.by_team.get_mut(team as usize) else {
            return;
        };
        let dist_to_any = 0.0; // folded into threat below
        let found = known_at.get(enemy.id).filter(|&at| at < board.len());
        if let Some(at) = found {
            let known = &mut board[at];
            if seen {
                known.sighting.last_pos = enemy.pos;
                known.sighting.last_vel = enemy.vel;
                known.sighting.last_seen_tick = tick;
                known.sighting.confidence = 1.0;
                known.sighting.source = source;
                known.threat = threat_of(enemy, dist_to_any, source == SightSource::DirectLos);
            }
        } else if seen {
            known_at.insert(enemy.id, board.len());
            board.push(KnownEnemy {
                entity: enemy.id,
                sighting: Sighting {
                    entity: enemy.id,
                    last_pos: enemy.pos,
                    last_vel: enemy.vel,
                    last_seen_tick: tick,
                    first_seen_tick: tick,
                    confidence: 1.0,
                    source,
                },
                threat: threat_of(enemy, dist_to_any, source == SightSource::DirectLos),
            });
        }
    }

    /// Total threat a team can see, for a quick "am I in danger" signal.
    pub fn visible_threat(&self, team: u8) -> f32 {
        self.for_team(team)
            .iter()
            .filter(|k| k.threat.can_reach_me)
            .map(|k| k.threat.dps)
            .sum()
    }
}

/// A unit observed by the perception pass.
#[derive(Debug, Clone, Copy)]
pub struct PerceptUnit {
    /// Its entity id (also its LOS re-check phase).
    pub id: u32,
    /// Its team.
    pub team: u8,
    /// Its eye position.
    pub pos: [f32; 3],
    /// How far it sees with a clear line (metres, in plan).
    pub sight_range_m: f32,
    /// Radar reach of this observer: 1.0 intact, 0.5 with damaged sensors.
    pub radar_scale: f32,
}

impl PerceptUnit {
    /// Time-slice the LOS re-check per unit: a unit only re-checks LOS on its phase
    /// (`tick % interval == id % interval`).
    pub fn los_recheck_needed(&self, tick: u64, interval_ticks: u32) -> bool {
        let interval = interval_ticks.max(1) as u64;
        tick % interval == self.id as u64 % interval
    }
}

/// An enemy unit offered to the board.
#[derive(Debug, Clone, Copy)]
pub struct PerceptEnemy {
    /// Its entity id.
    pub id: u32,
    /// Its team.
    pub team: u8,
    /// Its position.
    pub pos: [f32; 3],
    /// Its plan velocity (x, z).
    pub vel: [f32; 2],
    /// Its damage per second.
    pub dps: f32,
    /// Its weapon's `[min, max]` range band (metres).
    pub band: [f32; 2],
    /// Counts as lit for detection (headlights, a flare, an illumination round, a
    /// ping flash): radar reaches it at the lit range.
    pub lit: bool,
}

/// A contact a light or sensor pass reveals without line of sight or radar: a lit
/// hull, heat on a dark observer's thermal view, or a lidar sweep.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LitContact {
    /// Its entity id.
    pub id: u32,
    /// Its team.
    pub team: u8,
    /// Its position.
    pub pos: [f32; 3],
    /// Its damage per second.
    pub dps: f32,
    /// Its weapon's `[min, max]` range band (metres).
    pub band: [f32; 2],
}

/// Fold the enemy's weapon table, distance and firing solution into a
/// [`ThreatEstimate`]. `incoming` = it has LOS to us and is in its band.
fn threat_of(enemy: &PerceptEnemy, dist: f32, incoming: bool) -> ThreatEstimate {
    ThreatEstimate {
        dps: enemy.dps,
        range_band: enemy.band,
        can_reach_me: dist <= enemy.band[1] || enemy.band[1] <= 0.0,
        incoming,
    }
}

/// Entity id -> first row of one team's board, as the refresh keeps it while it upserts.
pub(super) trait KnownAt {
    /// The first row holding `id`.
    fn get(&self, id: u32) -> Option<usize>;
    /// Record `id` at row `at` (replacing an older entry for `id`).
    fn insert(&mut self, id: u32, at: usize);
}

impl KnownAt for std::collections::BTreeMap<u32, usize> {
    fn get(&self, id: u32) -> Option<usize> {
        std::collections::BTreeMap::get(self, &id).copied()
    }
    fn insert(&mut self, id: u32, at: usize) {
        std::collections::BTreeMap::insert(self, id, at);
    }
}

#[inline]
pub(super) fn dist_xz(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dz = a[2] - b[2];
    crate::math::sqrt(dx * dx + dz * dz)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::los::HeightfieldLos;

    /// DO27's default direct-fire weapon row (60 dps, 0-150 m), inlined.
    const DPS: f32 = 60.0;
    const BAND: [f32; 2] = [0.0, 150.0];

    fn flat_sampler() -> HeightfieldLos<impl Fn(f32, f32) -> f32> {
        HeightfieldLos(|_: f32, _: f32| 0.0)
    }

    fn unit(id: u32, team: u8, pos: [f32; 3]) -> PerceptUnit {
        PerceptUnit {
            id,
            team,
            pos,
            sight_range_m: 220.0,
            radar_scale: 1.0,
        }
    }

    fn enemy(id: u32, team: u8, pos: [f32; 3]) -> PerceptEnemy {
        PerceptEnemy {
            id,
            team,
            pos,
            vel: [0.0, 0.0],
            dps: DPS,
            band: BAND,
            lit: false,
        }
    }

    /// Per-team perception params for a 3-team board with LOS re-checked every
    /// tick (interval 1), so the sighting-source tests below are deterministic
    /// on tick 0 regardless of the observer's phase.
    fn params_los_every_tick() -> Vec<PerceptionParams> {
        let p = PerceptionParams {
            los_recheck_interval_ticks: 1,
            ..PerceptionParams::default()
        };
        vec![p; 3]
    }

    #[test]
    fn direct_los_sighting_is_shared_to_the_team() {
        let t = flat_sampler();
        let mut board = SightingsBoard::new(3, RADAR_RANGE_M);
        let units = [unit(1, 1, [0.0, 0.0, 0.0]), unit(2, 2, [0.0, 0.0, 500.0])];
        let enemies = [enemy(10, 2, [0.0, 0.0, 100.0])];
        board.refresh(Some(&t), 0, &units, &enemies, &params_los_every_tick());
        assert_eq!(board.for_team(1).len(), 1, "team 1 sees enemy 10");
        assert_eq!(board.for_team(1)[0].entity, 10);
        assert_eq!(board.for_team(1)[0].sighting.source, SightSource::DirectLos);
        assert!(board.for_team(1)[0].sighting.confidence > 0.99);
        // Team 2 never sees its own unit as an enemy.
        assert!(board.for_team(2).is_empty());
    }

    #[test]
    fn radar_contact_does_not_need_los() {
        let t = flat_sampler();
        let mut board = SightingsBoard::new(3, RADAR_RANGE_M);
        // Enemy 200 m away: within radar (400 m) but LOS requires sight range 220.
        let units = [unit(1, 1, [0.0, 0.0, 0.0])];
        let enemies = [enemy(10, 2, [0.0, 0.0, 200.0])];
        board.refresh(Some(&t), 0, &units, &enemies, &params_los_every_tick());
        let seen = board.for_team(1);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].sighting.source, SightSource::DirectLos); // 200 <= 220 sight range
    }

    #[test]
    fn damaged_sensors_shorten_radar() {
        let t = flat_sampler();
        // Enemy at 300 m: beyond sight (220) but inside radar (400).
        let enemies = [enemy(10, 2, [0.0, 0.0, 300.0])];
        let params = params_los_every_tick();
        let fit = PerceptUnit {
            id: 1,
            team: 1,
            pos: [0.0, 0.0, 0.0],
            sight_range_m: 220.0,
            radar_scale: 1.0,
        };
        let mut full = SightingsBoard::new(3, RADAR_RANGE_M);
        full.refresh(Some(&t), 0, &[fit], &enemies, &params);
        assert_eq!(full.for_team(1).len(), 1);
        assert_eq!(full.for_team(1)[0].sighting.source, SightSource::Radar);
        // Sensors offline halves radar to 200 m: the same contact is gone.
        let weak = PerceptUnit {
            radar_scale: 0.5,
            ..fit
        };
        let mut hurt = SightingsBoard::new(3, RADAR_RANGE_M);
        hurt.refresh(Some(&t), 0, &[weak], &enemies, &params);
        assert!(hurt.for_team(1).is_empty());
    }

    #[test]
    fn lit_enemy_shows_past_radar_range() {
        let t = flat_sampler();
        let mut board = SightingsBoard::new(3, RADAR_RANGE_M);
        let units = [unit(1, 1, [0.0, 0.0, 0.0])];
        // 450 m: past sight range (220 m) and past radar (400 m). Lit reach is
        // the base plus the lit bonus (150 m), so 550 m still sees it.
        let mut dark = enemy(10, 2, [0.0, 0.0, 450.0]);
        dark.lit = false;
        board.refresh(Some(&t), 0, &units, &[dark], &params_los_every_tick());
        assert!(board.for_team(1).is_empty(), "dark past radar: unseen");
        let mut lit = enemy(10, 2, [0.0, 0.0, 450.0]);
        lit.lit = true;
        board.refresh(Some(&t), 1, &units, &[lit], &params_los_every_tick());
        let seen = board.for_team(1);
        assert_eq!(seen.len(), 1, "lit past radar: radar contact");
        assert_eq!(seen[0].sighting.source, SightSource::Radar);
    }

    #[test]
    fn sighting_decays_and_is_forgotten() {
        let t = flat_sampler();
        let mut board = SightingsBoard::new(3, RADAR_RANGE_M);
        let units = [unit(1, 1, [0.0, 0.0, 0.0])];
        let enemies = [enemy(10, 2, [0.0, 0.0, 100.0])];
        let params = params_los_every_tick();
        board.refresh(Some(&t), 0, &units, &enemies, &params);
        assert_eq!(board.for_team(1).len(), 1);
        // Move the enemy out of radar range, then refresh without sighting it.
        let empty: [PerceptEnemy; 0] = [];
        let mut ticks = 1;
        while board.for_team(1).len() == 1 && ticks < 100 {
            board.refresh(Some(&t), ticks, &units, &empty, &params);
            ticks += 1;
        }
        assert_eq!(
            board.for_team(1).len(),
            0,
            "sighting forgotten after enough decay"
        );
    }

    #[test]
    fn the_lit_bonus_is_a_setting() {
        let t = flat_sampler();
        let units = [unit(1, 1, [0.0, 0.0, 0.0])];
        let mut lit = enemy(10, 2, [0.0, 0.0, 450.0]);
        lit.lit = true;
        let mut short = SightingsBoard::new(3, RADAR_RANGE_M).with_lit_bonus_m(25.0);
        assert_eq!(short.detection_range(true), RADAR_RANGE_M + 25.0);
        short.refresh(Some(&t), 0, &units, &[lit], &params_los_every_tick());
        assert!(short.for_team(1).is_empty(), "a 25 m bonus does not reach 450 m");
    }
}
