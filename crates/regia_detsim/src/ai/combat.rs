// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/ai/combat.rs, commit c9513623f, tag regia-absorb-4;
// ir-sim/src/ai/wingman.rs lines 484-505; ir-sim/src/pilots.rs lines 1648-1654), ADR-8012.
// Mark: adapt (combat.rs lines 1-909, plan row S-12, ADR-8516). Target priority, the Re-up rule, the fire seam, the
// escort V with the allied keep-out and the option-to-motor composition are DO27's code apart from rustfmt, doc
// comments and these named changes:
// - DO27's hull and weapon stem tables (game content) are not copied: `max_hull(stem)`, `target_value(stem)`, the
//   summed `weapon_dps(stem)`, the repair and supply stem lists and the per-stem projectile speed become per-unit
//   data of the read view (`AiUnitView::max_hull`, `target_value`, `weapon_dps`, `reup`; a slot's
//   `projectile_speed`). A contact that left the world view keeps DO27's default table row (500 hull points,
//   value 100: `DEFAULT_MAX_HULL`, `DEFAULT_TARGET_VALUE`); the lead fallback with no slot is DO27's default cannon
//   speed (`FALLBACK_PROJECTILE_SPEED`). `Target` carries the two table values instead of the stem, so it is `Copy`
//   and `TeamTargets` keeps no separate stats rows;
// - the entity class test becomes `is_unit`, the on-foot crew flag `on_foot`;
// - line of sight is any S-10 `LineOfSight` (DO27's `Option<&TerrainSampler>`; `None` is still open field), and the
//   fire gate reads S-11's `ShooterView` and `FireTarget` (`AiUnitView::shooter`); the slot choice uses the unit's
//   `mortar_min_range_m` setting (DO27's 90 m by default);
// - `LOW_HULL_PCT`, `LOW_AMMO_PCT` and `ammo_pct` (wingman.rs, which plan row S-13 ports) and `acquire_range`
//   (pilots.rs) move here, where the Re-up rule and target priority read them;
// - the std float `min`, `max` and `clamp` calls go through `regia_detsim::float::det_*` (ADR-8085);
//   `x % 2 == 0` is written `is_multiple_of(2)`; DO27's `#[allow(clippy::too_many_arguments)]` and
//   `#[allow(dead_code)]` are dropped (the functions take seven arguments and `nearest_enemy` is exported);
// - `run_from` carries a trace span (per-tick work over every unit).

//! Combat behaviours: the composition layer.
//!
//! It maps each committed tactic ([`OptionId`]) to motor and aim primitives, and owns target priority. Push, hold,
//! strafe, dogfight, kite, flank and retreat are each one composition of the motor ([`super::motor`]) and the fire
//! gate ([`super::fire_gate`]). The anti-feeding rules (a low-hull or dry unit breaks off to a Re-up point, a
//! retreat falls back toward support) are enforced here.
//!
//! - [`target_priority`]: `value / time_to_kill + threat × proximity`, with in-range threats first, a finish bonus
//!   for damaged targets, stale sightings ignored and the team's trusted focus id winning outright.
//! - [`fire_intent`] and [`unit_output`]: the reaction and sighting-trust gate, then the hard fire gate; the
//!   committed option's steer; the Re-up override ([`reup_needed`], [`reup_point`]).
//! - [`separate_from_allies`] and the escort V: escorts and regrouping units form a staggered V on the leader with a
//!   hard allied keep-out ([`ALLY_KEEPOUT_M`]), never a shared point.
//! - [`run`]: every unit's output in entity-id order, each with its own per-unit-per-tick [`AiRng`] stream.
//!
//! Determinism: fixed iteration order, per-unit-per-tick seeded RNG, no wall clock, no same-tick cross-unit reads.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::AiRng;
use super::aim::{FireIntent, FireTarget, WeaponRole, choose_slot_with, fire_gate};
use super::difficulty::{AiLevel, AimParams, DifficultyProfile};
use super::influence::flank_bearing;
use super::los::LineOfSight;
use super::motor::{self, Pursuit};
use super::perception::{KnownEnemy, SightingsBoard};
use super::tactics::Decision;
use super::utility::OptionId;
use super::view::{
    AiUnitOutput, AiUnitView, AiWorld, CoordState, DEFAULT_MAX_HULL, DEFAULT_TARGET_VALUE, ReupRole,
    SteerCommand,
};
use crate::float::{det_clamp, det_max, det_min};

/// Below this hull fraction a unit breaks off and turns for repair.
pub const LOW_HULL_PCT: f32 = 0.30;
/// At or below this ammo fraction (on any finite slot) a unit breaks off to re-arm.
pub const LOW_AMMO_PCT: f32 = 0.20;

/// Projectile speed (m/s) for the lead and stand-off when no slot fits the target: DO27's default cannon.
pub const FALLBACK_PROJECTILE_SPEED: f32 = 120.0;
/// Stand-off band edge (metres) when no slot fits the target.
pub const FALLBACK_BAND_EDGE_M: f32 = 60.0;

/// The worst ammo fraction over the unit's finite slots; 1.0 when nothing is finite (energy weapons, or no
/// weapons).
pub fn ammo_pct(unit: &AiUnitView) -> f32 {
    let mut worst: Option<f32> = None;
    for slot in unit.ammo.slots.iter().flatten() {
        if let Some((cur, max)) = slot.ammo {
            if max == 0 {
                continue;
            }
            let pct = cur as f32 / max as f32;
            worst = Some(worst.map_or(pct, |w| det_min(w, pct)));
        }
    }
    worst.unwrap_or(1.0)
}

/// How far (metres) a unit of `level` looks for targets: the proximity term of [`target_priority`] falls to zero
/// there.
pub fn acquire_range(level: AiLevel) -> f32 {
    match level {
        AiLevel::Easy => 80.0,
        AiLevel::Medium => 140.0,
        AiLevel::Hard => 220.0,
    }
}

/// A resolved hostile target for one unit: the board entry folded with the world view for its class, hull and
/// table values.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Target {
    /// Entity id.
    pub id: u32,
    /// Last sighted position.
    pub pos: [f32; 3],
    /// Last sighted velocity (XZ; Y is 0).
    pub vel: [f32; 3],
    /// A unit (`false`: a structure).
    pub is_unit: bool,
    /// The board's threat estimate, damage per second.
    pub dps: f32,
    /// Hull fraction left.
    pub hull_pct: f32,
    /// Full hull points.
    pub max_hull: f32,
    /// Target value.
    pub value: f32,
    /// Tick the sighting was first created (the reaction-delay clock).
    pub first_seen_tick: u64,
    /// Tick the sighting was last refreshed (the sighting-trust clock).
    pub last_seen_tick: u64,
}

/// One board entry folded with its world view (`None`: the enemy left the world view).
fn resolve_one(k: &KnownEnemy, view: Option<&AiUnitView>) -> Target {
    Target {
        id: k.entity,
        pos: k.sighting.last_pos,
        vel: [k.sighting.last_vel[0], 0.0, k.sighting.last_vel[1]],
        is_unit: view.map(|u| u.is_unit).unwrap_or(true),
        dps: k.threat.dps,
        hull_pct: view.map(|u| u.hull_pct).unwrap_or(1.0),
        max_hull: view.map(|u| u.max_hull).unwrap_or(DEFAULT_MAX_HULL),
        value: view.map(|u| u.target_value).unwrap_or(DEFAULT_TARGET_VALUE),
        first_seen_tick: k.sighting.first_seen_tick,
        last_seen_tick: k.sighting.last_seen_tick,
    }
}

/// Every team's resolved targets, built once per run.
///
/// The resolved list depends on the unit's team only, so it is built once instead of once per unit (a linear search
/// of the world for every known enemy, per unit, per tick). The rows and their order are the board's.
#[derive(Debug, Clone, PartialEq)]
pub struct TeamTargets {
    by_team: Vec<Vec<Target>>,
    /// `(id, position)` of every world unit of a team, in world order (the friendly-fire cone reads them).
    allies: BTreeMap<u8, Vec<(u32, [f32; 3])>>,
}

impl TeamTargets {
    /// Targets for every team on the board.
    pub fn build(world: &AiWorld, board: &SightingsBoard) -> Self {
        Self::build_for(world, board, None)
    }

    /// Targets for one unit's team only (single-unit callers and tests).
    pub fn for_unit(unit: &AiUnitView, world: &AiWorld, board: &SightingsBoard) -> Self {
        Self::build_for(world, board, Some(unit.team))
    }

    fn build_for(world: &AiWorld, board: &SightingsBoard, only: Option<u8>) -> Self {
        // The first unit of each id, which is what a forward `find` returns.
        let mut first: BTreeMap<u32, usize> = BTreeMap::new();
        for (index, unit) in world.units.iter().enumerate() {
            first.entry(unit.id).or_insert(index);
        }
        let by_team = board
            .by_team
            .iter()
            .enumerate()
            .map(|(team, known)| {
                if only.is_some_and(|only| usize::from(only) != team) {
                    return Vec::new();
                }
                known
                    .iter()
                    .map(|k| resolve_one(k, first.get(&k.entity).map(|&index| &world.units[index])))
                    .collect()
            })
            .collect();
        let mut allies: BTreeMap<u8, Vec<(u32, [f32; 3])>> = BTreeMap::new();
        for unit in &world.units {
            if only.is_none_or(|only| only == unit.team) {
                allies.entry(unit.team).or_default().push((unit.id, unit.pos));
            }
        }
        TeamTargets { by_team, allies }
    }

    /// The resolved targets of `team` (empty for a team the board does not hold).
    pub fn for_team(&self, team: u8) -> &[Target] {
        self.by_team.get(team as usize).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// Finish-the-damaged bonus: a target at 0 hull scores twice a full-hull target with the same value and
/// time-to-kill, so the AI finishes kills instead of spreading damage across fresh targets.
pub const FINISH_BONUS: f32 = 1.0;

/// True when the unit must break off and Re-up (a standing rule): hull at or below [`LOW_HULL_PCT`], or any finite
/// ammo slot at or below [`LOW_AMMO_PCT`]. Deterministic.
pub fn reup_needed(hull_pct: f32, ammo_pct: f32) -> bool {
    hull_pct <= LOW_HULL_PCT || ammo_pct <= LOW_AMMO_PCT
}

/// Nearest allied Re-up point for a unit that must break off: the nearest [`ReupRole::Repair`] point while the hull
/// is low (repair wins, a dead unit banks nothing), else the nearest [`ReupRole::Supply`] point while ammo is dry,
/// else whichever Re-up point is nearest. `None` when the team holds no Re-up point. Deterministic: world iteration
/// is entity-id ordered and ties keep the first.
pub fn reup_point(unit: &AiUnitView, world: &AiWorld) -> Option<[f32; 3]> {
    let ammo = ammo_pct(unit);
    let repair = nearest_reup(unit, world, ReupRole::Repair);
    let supply = nearest_reup(unit, world, ReupRole::Supply);
    if unit.hull_pct <= LOW_HULL_PCT {
        repair.or(supply)
    } else if ammo <= LOW_AMMO_PCT {
        supply.or(repair)
    } else {
        match (repair, supply) {
            (Some(r), Some(s)) => {
                if dist_xz(unit.pos, r) <= dist_xz(unit.pos, s) {
                    Some(r)
                } else {
                    Some(s)
                }
            }
            (Some(r), None) => Some(r),
            (None, Some(s)) => Some(s),
            (None, None) => None,
        }
    }
}

/// Nearest living allied entity with the Re-up `role`, or `None`.
fn nearest_reup(unit: &AiUnitView, world: &AiWorld, role: ReupRole) -> Option<[f32; 3]> {
    let mut best: Option<(f32, [f32; 3])> = None;
    for u in &world.units {
        if u.team != unit.team || u.id == unit.id {
            continue;
        }
        if u.reup != role {
            continue;
        }
        let d = dist_xz(unit.pos, u.pos);
        let better = best.is_none_or(|(bd, _)| d < bd);
        if better {
            best = Some((d, u.pos));
        }
    }
    best.map(|(_, p)| p)
}

/// True when any usable weapon slot can engage a target at its range: the range sits inside the slot's band
/// (seekers only count against units, mortars only against structures: the same role rule as the slot choice).
/// Targets outside every band are out-of-range threats, deprioritised by [`target_priority`] but never
/// unfireable-on-approach (the unit still closes on them).
pub fn target_in_weapon_band(unit: &AiUnitView, target_pos: [f32; 3], target_is_unit: bool) -> bool {
    let range = dist_xz(unit.pos, target_pos);
    unit.ammo.slots.iter().flatten().any(|s| {
        let usable = match s.ammo {
            None => true,
            Some((cur, _)) => cur > 0,
        };
        if !usable || range < s.band[0] || range > s.band[1] {
            return false;
        }
        match s.role {
            WeaponRole::Seeker => target_is_unit,
            WeaponRole::Mortar => !target_is_unit,
            WeaponRole::Direct => true,
        }
    })
}

/// Target priority: `value / time_to_kill + threat × proximity`, so a low-health high-DPS enemy beats a distant
/// valuable structure, while a trusted coordinator focus id ("attack my target") wins outright. Three deterministic
/// rules sit on top:
///
/// 1. Threats in weapon range sort before threats outside every band: the AI answers what can hit it now instead of
///    chasing a far valuable contact.
/// 2. Damaged targets score a finish bonus (`1 + (1 - hull_pct)`), so the AI finishes kills instead of spreading
///    damage across fresh targets.
/// 3. Stale sightings (older than the difficulty's `sighting_trust_ticks`) are not targets at all: the AI holds
///    instead of chasing ghosts into the fog. The focus id wins only while it is trusted.
///
/// Ties within `1e-6` go to the lower entity id.
pub fn target_priority(
    unit: &AiUnitView,
    world: &AiWorld,
    board: &SightingsBoard,
    coord: &CoordState,
) -> Option<Target> {
    target_priority_from(unit, world, &TeamTargets::for_unit(unit, world, board), coord)
}

/// [`target_priority`] on targets resolved once for the whole run ([`TeamTargets`]).
pub fn target_priority_from(
    unit: &AiUnitView,
    world: &AiWorld,
    targets: &TeamTargets,
    coord: &CoordState,
) -> Option<Target> {
    let our_dps = det_max(unit.weapon_dps, 1.0);
    let range = acquire_range(unit.level);
    let trust = DifficultyProfile::for_level(unit.level)
        .perception
        .sighting_trust_ticks as u64;
    let resolved = targets.for_team(unit.team);
    // What `target_in_weapon_band` reads off each of this unit's slots, read once instead of once per target:
    // (usable, band, role).
    let slots: Vec<(bool, [f32; 2], WeaponRole)> = unit
        .ammo
        .slots
        .iter()
        .flatten()
        .map(|s| {
            let usable = match s.ammo {
                None => true,
                Some((cur, _)) => cur > 0,
            };
            (usable, s.band, s.role)
        })
        .collect();
    // The focus id wins outright while its sighting is trusted.
    if let Some(focus_id) = coord.focus_id(unit.team)
        && let Some(t) = resolved.iter().find(|t| t.id == focus_id && t.id != unit.id)
        && world.tick.saturating_sub(t.last_seen_tick) <= trust
    {
        return Some(*t);
    }
    // Cheapest tier first: an in-range target always beats an out-of-range one, so score only decides within a
    // tier.
    let mut best: Option<(bool, f32, &Target)> = None;
    for t in resolved {
        if t.id == unit.id {
            continue;
        }
        if world.tick.saturating_sub(t.last_seen_tick) > trust {
            continue; // stale: do not chase into the fog
        }
        let remaining = t.max_hull * det_max(t.hull_pct, 0.01);
        let ttk = remaining / our_dps;
        let dx = t.pos[0] - unit.pos[0];
        let dz = t.pos[2] - unit.pos[2];
        let dist = crate::math::sqrt(dx * dx + dz * dz);
        let proximity = det_clamp(1.0 - (dist / det_max(range, 1.0)), 0.0, 1.0);
        let mut score = t.value / det_max(ttk, 0.01) + t.dps * proximity;
        score *= 1.0 + FINISH_BONUS * (1.0 - det_clamp(t.hull_pct, 0.0, 1.0));
        // `target_in_weapon_band(unit, t.pos, t.is_unit)`, on the slot rows above.
        let band_range = dist_xz(unit.pos, t.pos);
        let in_range = slots.iter().any(|(usable, band, role)| {
            if !usable || band_range < band[0] || band_range > band[1] {
                return false;
            }
            match role {
                WeaponRole::Seeker => t.is_unit,
                WeaponRole::Mortar => !t.is_unit,
                WeaponRole::Direct => true,
            }
        });
        let better = match &best {
            None => true,
            Some((best_in, best_score, best_t)) => {
                if in_range != *best_in {
                    in_range
                } else if (score - *best_score).abs() > 1.0e-6 {
                    score > *best_score
                } else {
                    t.id < best_t.id
                }
            }
        };
        if better {
            best = Some((in_range, score, t));
        }
    }
    best.map(|(_, _, t)| *t)
}

/// The wingman fire seam: pick a target (restricted to `restrict` when a commander names one, "attack my target")
/// and run it through the reaction and sighting-trust gate and the hard fire gate. Movement is the caller's; the
/// shot is the combat layer's. Deterministic: same inputs, same intent.
pub fn fire_intent<L: LineOfSight + Copy>(
    unit: &AiUnitView,
    world: &AiWorld,
    board: &SightingsBoard,
    coord: &CoordState,
    los: L,
    rng: &mut AiRng,
    restrict: Option<u32>,
) -> Option<FireIntent> {
    let targets = TeamTargets::for_unit(unit, world, board);
    fire_intent_from(unit, world, &targets, coord, los, rng, restrict)
}

/// [`fire_intent`] on targets resolved once for the whole run ([`TeamTargets`]).
pub fn fire_intent_from<L: LineOfSight + Copy>(
    unit: &AiUnitView,
    world: &AiWorld,
    targets: &TeamTargets,
    coord: &CoordState,
    los: L,
    rng: &mut AiRng,
    restrict: Option<u32>,
) -> Option<FireIntent> {
    let profile = DifficultyProfile::for_level(unit.level);
    let friendlies = friendlies_from(unit, targets);
    let target = match restrict {
        Some(id) => targets.for_team(unit.team).iter().find(|t| t.id == id).copied(),
        None => target_priority_from(unit, world, targets, coord),
    };
    let target = target?;
    // Reaction + sighting-trust gate (the same difficulty decision quality as `unit_output`): no snapshot shots at
    // a contact the unit has not reacted to, and no firing at a stale sighting through the fog.
    let reacted = world.tick.saturating_sub(target.first_seen_tick) >= profile.reaction_delay_ticks as u64;
    let trusted =
        world.tick.saturating_sub(target.last_seen_tick) <= profile.perception.sighting_trust_ticks as u64;
    if !(reacted && trusted) {
        return None;
    }
    fire_at(unit, &target, &profile.aim, rng, los, &friendlies)
}

/// Nearest known enemy of the unit's team (board order breaks ties), for a caller with no stronger preference.
pub fn nearest_enemy<'a>(unit: &AiUnitView, board: &'a SightingsBoard) -> Option<&'a KnownEnemy> {
    board.for_team(unit.team).iter().min_by(|a, b| {
        let da = dist_xz(a.sighting.last_pos, unit.pos);
        let db = dist_xz(b.sighting.last_pos, unit.pos);
        da.partial_cmp(&db).unwrap_or(core::cmp::Ordering::Equal)
    })
}

#[inline]
fn dist_xz(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dz = a[2] - b[2];
    crate::math::sqrt(dx * dx + dz * dz)
}

/// Positions of same-team allies (excluding `unit`), for the friendly-fire cone, in world (entity-id) order.
fn friendlies_from(unit: &AiUnitView, targets: &TeamTargets) -> Vec<[f32; 3]> {
    targets
        .allies
        .get(&unit.team)
        .map(|team| {
            team.iter()
                .filter(|(id, _)| *id != unit.id)
                .map(|(_, pos)| *pos)
                .collect()
        })
        .unwrap_or_default()
}

/// Fighting-withdrawal steer (fall back while firing): head for the nearest Re-up point when the team holds one
/// (repair first), else the ally centroid, else directly away from the threat while keeping the guns on it. Needs
/// no live target: `None` still walks to the Re-up point or support.
fn retreat_steer(unit: &AiUnitView, world: &AiWorld, target: Option<&Target>) -> SteerCommand {
    if let Some(rp) = reup_point(unit, world) {
        return motor::arrive(unit, rp, 6.0);
    }
    if let Some(sp) = support_point(unit, world) {
        return motor::arrive(unit, sp, 6.0);
    }
    match target {
        Some(t) => motor::evade(unit, t.pos),
        None => SteerCommand {
            throttle: 0.0,
            strafe: 0.0,
            steer: 0.0,
        },
    }
}

/// Team centroid of living allies (the support point a retreat heads toward).
fn support_point(unit: &AiUnitView, world: &AiWorld) -> Option<[f32; 3]> {
    let allies: Vec<&AiUnitView> = world
        .units
        .iter()
        .filter(|u| u.team == unit.team && u.id != unit.id)
        .collect();
    if allies.is_empty() {
        return None;
    }
    let n = allies.len() as f32;
    let mut sum = [0.0f32; 3];
    for a in allies {
        sum[0] += a.pos[0];
        sum[1] += a.pos[1];
        sum[2] += a.pos[2];
    }
    Some([sum[0] / n, sum[1] / n, sum[2] / n])
}

// ---------------------------------------------------------------------------
// Escort / regroup formation
//
// Escorts steer to a per-wingman slot of a staggered V on the leader, never one shared point (the team centroid),
// with a hard allied keep-out enforced by separation steering before any arrive. Ten wingmen steering to one point
// converge on the commander's hull and fill a cockpit view.
// ---------------------------------------------------------------------------

/// Hard keep-out margin (metres) between two allied hull centres: the radius sum plus this margin. An allied hull
/// never parks closer than that to the commander's hull centre.
pub const ALLY_KEEPOUT_M: f32 = 2.0;
/// Separation triggers a little beyond the keep-out so a hull settles strictly clear of it, not on the float knife
/// edge at the boundary.
const KEEPOUT_HYSTERESIS_M: f32 = 0.25;
/// How far an escort parks from its slot before it counts as "arrived".
const ESCORT_ARRIVE_M: f32 = 2.5;

/// Centre-to-centre keep-out distance (metres) between two allied hulls: `a_radius + b_radius + ALLY_KEEPOUT_M`.
#[inline]
pub fn ally_keepout_m(a_radius: f32, b_radius: f32) -> f32 {
    a_radius + b_radius + ALLY_KEEPOUT_M
}

/// The hulls that count for allied separation: same team, a unit (not a structure), not crew on foot (a hull must
/// still approach crew to pick them up). Iterated in entity-id order.
fn allied_hulls<'a>(unit: &AiUnitView, world: &'a AiWorld) -> impl Iterator<Item = &'a AiUnitView> {
    let (team, id) = (unit.team, unit.id);
    world
        .units
        .iter()
        .filter(move |u| u.team == team && u.id != id && u.is_unit && !u.on_foot)
}

/// Separation steering (before arrive): when an allied hull sits inside the keep-out, steer out along the weighted
/// sum of the away-directions of every violator (not just the nearest: evading one hull must not push into the
/// next). The commander's hull keeps the full margin ([`ally_keepout_m`]); any other allied hull only needs
/// no-overlap, so a formed group is not kicked apart by transient brush-pasts. The away vector is applied on this
/// tick as throttle and strafe, so a hull whose nose still points at the commander slides clear instead of turning
/// in place. `None` when no allied hull is inside its keep-out. Deterministic: fixed entity-id iteration order,
/// `crate::math` only.
pub fn separate_from_allies(unit: &AiUnitView, world: &AiWorld) -> Option<SteerCommand> {
    let mut ax = 0.0f32;
    let mut az = 0.0f32;
    // Deepest violator, for the near-cancellation fallback (a wingman squeezed between the leader and another escort
    // can see their away-directions nearly cancel; the sum must not become a garbage heading). (depth, id, away unit
    // vector).
    let mut worst: Option<(f32, u32, f32, f32)> = None;
    for ally in allied_hulls(unit, world) {
        let dx = unit.pos[0] - ally.pos[0];
        let dz = unit.pos[2] - ally.pos[2];
        let d = crate::math::sqrt(dx * dx + dz * dz);
        let keep = if ally.is_commander {
            ally_keepout_m(unit.radius, ally.radius)
        } else {
            unit.radius + ally.radius + 0.5
        };
        let trigger = keep + KEEPOUT_HYSTERESIS_M;
        if d >= trigger {
            continue;
        }
        // Dead-centre coincidents break along +-X by entity id, so both hulls of a pair always push opposite ways.
        let (ux, uz) = if d > 1.0e-3 {
            (dx / d, dz / d)
        } else if unit.id < ally.id {
            (1.0, 0.0)
        } else {
            (-1.0, 0.0)
        };
        let depth = (trigger - d) / trigger;
        let weight = det_max(depth, 0.1);
        ax += ux * weight;
        az += uz * weight;
        let better = worst.is_none_or(|(wd, wid, _, _)| (depth, ally.id) > (wd, wid));
        if better {
            worst = Some((depth, ally.id, ux, uz));
        }
    }
    let (_, _, wx, wz) = worst?;
    // Near-cancellation: escape directly away from the deepest violator.
    let mag = crate::math::sqrt(ax * ax + az * az);
    let (ex, ez) = if mag < 0.05 {
        (wx, wz)
    } else {
        (ax / mag, az / mag)
    };
    let desired = crate::math::atan2(ex, ez);
    // Project the away vector onto the hull axes. Reverse and strafe move the hull out on this tick even when the
    // nose still points at the commander. A zero throttle while the nose comes around is what left a wingman filling
    // the view for several seconds.
    let (s, c) = (crate::math::sin(unit.yaw), crate::math::cos(unit.yaw));
    let fwd = det_clamp(ex * s + ez * c, -1.0, 1.0);
    let side = det_clamp(ex * c - ez * s, -1.0, 1.0);
    Some(SteerCommand {
        throttle: fwd,
        strafe: side,
        steer: motor::steer_to(unit.yaw, desired),
    })
}

/// The leader an escort/regroup unit forms on: the team's commander hull, else the lowest-entity-id allied hull
/// (the formation leader). `None` when alone.
fn escort_leader<'a>(unit: &AiUnitView, world: &'a AiWorld) -> Option<&'a AiUnitView> {
    allied_hulls(unit, world)
        .find(|u| u.is_commander)
        .or_else(|| allied_hulls(unit, world).min_by_key(|u| u.id))
}

/// This unit's slot rank among the escorts formed on the same team (count of allied hull ids below its own, the
/// leader excluded): the "slot index by unit id" rule, computed order-independently so it is network-deterministic.
fn escort_rank(unit: &AiUnitView, world: &AiWorld, leader_id: u32) -> usize {
    allied_hulls(unit, world)
        .filter(|u| u.id < unit.id && u.id != leader_id)
        .count()
}

/// The staggered-V slot for escort `rank` on `leader`, rotated with the leader's heading (forward =
/// `(sin yaw, cos yaw)`, right = `(cos yaw, -sin yaw)`). Even ranks fly the right wing, odd ranks the left; every
/// file sits behind the leader and outside its forward fire lane. Spacing: file step >= `2 x (own radius + leader
/// radius) + 4 m`, and the first file stays laterally beyond the keep-out plus one hull, so no slot can park an
/// escort on the leader or ahead of its guns.
fn escort_slot(unit: &AiUnitView, leader: &AiUnitView, rank: usize) -> [f32; 3] {
    let file_step = 2.0 * (unit.radius + leader.radius) + 4.0;
    let lane = ally_keepout_m(unit.radius, leader.radius) + unit.radius + 2.0;
    let side = if rank.is_multiple_of(2) { 1.0 } else { -1.0 };
    let file = (rank / 2) as f32;
    let (right, forward) = (side * lane, -file_step * (file + 1.0));
    let (s, c) = (crate::math::sin(leader.yaw), crate::math::cos(leader.yaw));
    [
        leader.pos[0] + s * forward + c * right,
        leader.pos[1],
        leader.pos[2] + c * forward - s * right,
    ]
}

/// True when the segment `a -> b` stays at least `keep` metres from `centre` in the XZ plane. A zero-length segment
/// tests `a` itself.
fn segment_clears(a: [f32; 3], b: [f32; 3], centre: [f32; 3], keep: f32) -> bool {
    let abx = b[0] - a[0];
    let abz = b[2] - a[2];
    let apx = centre[0] - a[0];
    let apz = centre[2] - a[2];
    let ab2 = abx * abx + abz * abz;
    let t = if ab2 < 1.0e-6 {
        0.0
    } else {
        det_clamp((apx * abx + apz * abz) / ab2, 0.0, 1.0)
    };
    let cx = a[0] + abx * t - centre[0];
    let cz = a[2] + abz * t - centre[2];
    crate::math::sqrt(cx * cx + cz * cz) >= keep
}

/// The point an escort steers to this tick: its V slot, or an abeam gate when the straight line to the slot would
/// cut through the leader's keep-out. Wingmen that spawn ahead of the leader go wide and then back, instead of
/// driving up its nose.
pub(crate) fn escort_goal(unit: &AiUnitView, leader: &AiUnitView, rank: usize) -> [f32; 3] {
    let slot = escort_slot(unit, leader, rank);
    let keep = ally_keepout_m(unit.radius, leader.radius);
    if segment_clears(unit.pos, slot, leader.pos, keep) {
        return slot;
    }
    let lane = keep + unit.radius + 2.0;
    let (s, c) = (crate::math::sin(leader.yaw), crate::math::cos(leader.yaw));
    let udx = unit.pos[0] - leader.pos[0];
    let udz = unit.pos[2] - leader.pos[2];
    let unit_right = udx * c - udz * s;
    let side = if unit_right.abs() > 1.0 {
        if unit_right >= 0.0 { 1.0 } else { -1.0 }
    } else {
        let dx = slot[0] - leader.pos[0];
        let dz = slot[2] - leader.pos[2];
        let slot_right = dx * c - dz * s;
        if slot_right >= 0.0 { 1.0 } else { -1.0 }
    };
    let forward = -2.0;
    let right = side * lane;
    [
        leader.pos[0] + s * forward + c * right,
        leader.pos[1],
        leader.pos[2] + c * forward - s * right,
    ]
}

/// The escort/regroup steer: separation first (the hard allied keep-out, which covers the commander's hull), then
/// arrive at this unit's own slot of the V on the leader, with the leader's velocity as feedforward so a wingman
/// holds station on a moving commander instead of stop-and-go.
fn escort_steer(unit: &AiUnitView, world: &AiWorld) -> Option<SteerCommand> {
    if let Some(separate) = separate_from_allies(unit, world) {
        return Some(separate);
    }
    let leader = escort_leader(unit, world)?;
    let slot = escort_goal(unit, leader, escort_rank(unit, world, leader.id));
    Some(motor::arrive_moving(unit, slot, ESCORT_ARRIVE_M, leader.vel))
}

/// Fire at `target` through the hard gate, if the unit is weapons-free and the shot is legal. Returns a
/// [`FireIntent`] with human-like aim error, or `None`.
fn fire_at<L: LineOfSight>(
    unit: &AiUnitView,
    target: &Target,
    aim: &AimParams,
    rng: &mut AiRng,
    los: L,
    friendlies: &[[f32; 3]],
) -> Option<FireIntent> {
    let range = dist_xz(unit.pos, target.pos);
    let target = FireTarget {
        id: target.id,
        pos: target.pos,
        vel: target.vel,
        is_unit: target.is_unit,
        range,
    };
    fire_gate(&unit.shooter(), &target, aim, rng, los, friendlies)
}

/// Stand-off point at `band_edge` metres along the line to `target`: where a Hold/snipe unit parks so it stays at
/// its weapon's best range.
fn standoff_point(unit: &AiUnitView, target: &Target, band_edge: f32) -> [f32; 3] {
    let dx = target.pos[0] - unit.pos[0];
    let dz = target.pos[2] - unit.pos[2];
    let dist = det_max(crate::math::sqrt(dx * dx + dz * dz), 1.0e-3);
    [
        target.pos[0] - dx / dist * band_edge,
        unit.pos[1],
        target.pos[2] - dz / dist * band_edge,
    ]
}

/// Expand one unit's committed option into steer and fire.
///
/// - A unit at or below the Re-up thresholds ([`reup_needed`]) retreats whatever it decided, still firing.
/// - Regroup and Escort form on the leader's V ([`separate_from_allies`] first); Defend and CollectScrap hold (the
///   host fills them).
/// - With no live target only a retreat moves (to the Re-up point, else support).
/// - Otherwise the reaction and sighting-trust gate, then the fire gate, then the option's steer: Push and FocusFire
///   lead-pursue, Hold parks at the stand-off (85 % of the chosen weapon's far band edge), Strafe, Dogfight and Kite
///   orbit, Flank arrives at a point 40 m along [`flank_bearing`], Retreat withdraws.
pub fn unit_output<L: LineOfSight + Copy>(
    unit: &AiUnitView,
    world: &AiWorld,
    board: &SightingsBoard,
    coord: &CoordState,
    los: L,
    decision: Option<OptionId>,
    rng: &mut AiRng,
) -> AiUnitOutput {
    // An undecided unit never resolved its targets.
    if decision.is_none() {
        return AiUnitOutput::default();
    }
    let targets = TeamTargets::for_unit(unit, world, board);
    unit_output_from(unit, world, &targets, coord, los, decision, rng)
}

/// [`unit_output`] on targets resolved once for the whole run ([`TeamTargets`]).
pub fn unit_output_from<L: LineOfSight + Copy>(
    unit: &AiUnitView,
    world: &AiWorld,
    targets: &TeamTargets,
    coord: &CoordState,
    los: L,
    decision: Option<OptionId>,
    rng: &mut AiRng,
) -> AiUnitOutput {
    let Some(opt) = decision else {
        return AiUnitOutput::default();
    };
    // Re-up override (a standing rule): a unit at or below the hull/ammo thresholds breaks off whatever it was doing
    // and makes a fighting withdrawal toward a Re-up point. Fire posture is untouched: it falls back while firing.
    let opt = if reup_needed(unit.hull_pct, ammo_pct(unit)) {
        OptionId::Retreat
    } else {
        opt
    };
    let profile = DifficultyProfile::for_level(unit.level);
    let friendlies = friendlies_from(unit, targets);
    let target = target_priority_from(unit, world, targets, coord);
    let mut out = AiUnitOutput::default();

    // Options that need no target: regroup/escort form on the leader's V (per-wingman slot + hard allied keep-out,
    // never the team centroid), and the host-filled options hold their ground (no spin, no fire).
    match opt {
        OptionId::Regroup | OptionId::Escort => {
            out.steer = escort_steer(unit, world);
            return out;
        }
        OptionId::Defend | OptionId::CollectScrap => return out,
        _ => {}
    }

    let Some(target) = target else {
        // No live target: a fighting withdrawal still walks (to the Re-up point, else to support) instead of
        // stopping in the open. Every other option holds with no shot.
        if opt == OptionId::Retreat {
            out.steer = Some(retreat_steer(unit, world, None));
        }
        return out;
    };

    let aim = &profile.aim;
    let lead_time = dist_xz(unit.pos, target.pos) / chosen_speed(unit, &target);
    let band_edge = chosen_band(unit, &target);

    // Reaction + sighting-trust gate (difficulty's decision quality): a unit does not open fire until
    // `reaction_delay_ticks` after it first acquired the target, and it only shoots a target whose last sighting is
    // within `sighting_trust_ticks` (a Hard bot tracks through brief line-of-sight loss, an Easy bot forgets sooner).
    // Movement still proceeds while the unit reacts.
    let reacted = world.tick.saturating_sub(target.first_seen_tick) >= profile.reaction_delay_ticks as u64;
    let trusted =
        world.tick.saturating_sub(target.last_seen_tick) <= profile.perception.sighting_trust_ticks as u64;
    out.fire = if reacted && trusted {
        fire_at(unit, &target, aim, rng, los, &friendlies)
    } else {
        None
    };

    let side = if unit.id.is_multiple_of(2) { 1.0 } else { -1.0 };
    let steer = match opt {
        OptionId::Push | OptionId::FocusFire => {
            motor::pursue(unit, target.pos, target.vel, Pursuit::Lead, lead_time)
        }
        OptionId::Hold => motor::arrive(unit, standoff_point(unit, &target, band_edge), 6.0),
        OptionId::Strafe => motor::orbit(unit, target.pos, det_min(band_edge, 50.0), side),
        // Lag pursuit + lateral orbit (the hover dogfight move): turn inside the target by strafing around it while
        // keeping guns on it.
        OptionId::Dogfight => motor::orbit(unit, target.pos, 40.0, side),
        // Keep range at the band edge while strafing laterally to dodge return fire: the ranged sibling of the strafe
        // orbit.
        OptionId::Kite => motor::orbit(unit, target.pos, det_min(band_edge, 70.0), side),
        OptionId::Flank => {
            let yaw = world
                .units
                .iter()
                .find(|u| u.id == target.id)
                .map(|u| u.yaw)
                .unwrap_or(0.0);
            let bearing = flank_bearing(los, unit.pos, target.pos, yaw);
            let point = [
                unit.pos[0] + crate::math::sin(bearing) * 40.0,
                unit.pos[1],
                unit.pos[2] + crate::math::cos(bearing) * 40.0,
            ];
            motor::arrive(unit, point, 4.0)
        }
        OptionId::Retreat => retreat_steer(unit, world, Some(&target)),
        _ => SteerCommand {
            throttle: 0.0,
            strafe: 0.0,
            steer: 0.0,
        },
    };
    out.steer = Some(steer);
    out
}

/// The projectile speed of the slot the fire gate would choose against `target` (for the lead time), or
/// [`FALLBACK_PROJECTILE_SPEED`] when none fits.
fn chosen_speed(unit: &AiUnitView, target: &Target) -> f32 {
    let range = dist_xz(unit.pos, target.pos);
    choose_slot_with(&unit.ammo, range, target.is_unit, unit.mortar_min_range_m)
        .and_then(|i| unit.ammo.slots.get(i).and_then(|s| s.as_ref()))
        .map(|s| s.projectile_speed)
        .unwrap_or(FALLBACK_PROJECTILE_SPEED)
}

/// The best range for the chosen weapon: 85 % of the far edge of its band (Hold/snipe and Kite park there; Strafe
/// orbits there), or [`FALLBACK_BAND_EDGE_M`] when no slot fits.
fn chosen_band(unit: &AiUnitView, target: &Target) -> f32 {
    let range = dist_xz(unit.pos, target.pos);
    choose_slot_with(&unit.ammo, range, target.is_unit, unit.mortar_min_range_m)
        .and_then(|i| unit.ammo.slots.get(i).and_then(|s| s.as_ref()))
        .map(|s| s.band[1] * 0.85)
        .unwrap_or(FALLBACK_BAND_EDGE_M)
}

/// Run the combat layer for every unit: one [`AiUnitOutput`] per unit, in the world's (entity-id) order, to apply
/// after all decisions. Each unit draws from its own per-unit-per-tick stream ([`AiRng::tick_stream`]).
pub fn run<L: LineOfSight + Copy>(
    world: &AiWorld,
    board: &SightingsBoard,
    coord: &CoordState,
    los: L,
    decisions: &BTreeMap<u32, Decision>,
) -> Vec<AiUnitOutput> {
    run_from(world, &TeamTargets::build(world, board), coord, los, decisions)
}

/// [`run`] on targets resolved once for the whole run ([`TeamTargets`]).
pub fn run_from<L: LineOfSight + Copy>(
    world: &AiWorld,
    targets: &TeamTargets,
    coord: &CoordState,
    los: L,
    decisions: &BTreeMap<u32, Decision>,
) -> Vec<AiUnitOutput> {
    let _span = tracing::trace_span!(
        "regia_detsim::ai::combat",
        tick = world.tick,
        units = world.units.len()
    )
    .entered();
    let mut outputs = Vec::with_capacity(world.units.len());
    for unit in &world.units {
        let decision = decisions.get(&unit.id).and_then(|d| d.current);
        let mut rng = AiRng::new(AiRng::mix(
            world.world_seed ^ world.tick.wrapping_mul(0x9E37_79B9_7F4A_7C15),
            unit.id as u64,
        ));
        outputs.push(unit_output_from(
            unit, world, targets, coord, los, decision, &mut rng,
        ));
    }
    outputs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AmmoSlotView, OpenField};

    fn cannon(ammo: Option<(u32, u32)>) -> AmmoSlotView {
        AmmoSlotView {
            role: WeaponRole::Direct,
            projectile_speed: 120.0,
            ammo,
            band: [0.0, 150.0],
        }
    }

    /// Not in DO27: the worst finite fraction wins, energy slots and zero-capacity slots never count.
    #[test]
    fn ammo_pct_reads_the_worst_finite_slot() {
        let mut u = AiUnitView::new(1, 1, [0.0; 3]);
        assert_eq!(ammo_pct(&u), 1.0, "no weapons: full");
        u.ammo.slots = vec![Some(cannon(None)), Some(cannon(Some((0, 0)))), None];
        assert_eq!(ammo_pct(&u), 1.0, "energy and zero-capacity slots never run dry");
        u.ammo.slots.push(Some(cannon(Some((30, 100)))));
        u.ammo.slots.push(Some(cannon(Some((50, 100)))));
        assert_eq!(ammo_pct(&u), 0.3);
    }

    /// Not in DO27: `run` gives one output per unit in world order, each from the unit's own tick stream, so it
    /// matches `unit_output` called with `AiRng::tick_stream`.
    #[test]
    fn run_matches_unit_output_on_the_tick_stream() {
        let mut a = AiUnitView::new(1, 1, [0.0; 3]);
        a.ammo.slots = vec![Some(cannon(None))];
        a.weapon_dps = 60.0;
        a.level = AiLevel::Hard;
        let mut b = AiUnitView::new(5, 2, [0.0, 0.0, 60.0]);
        b.ammo = a.ammo.clone();
        b.level = AiLevel::Hard;
        let world = AiWorld {
            tick: 40,
            world_seed: 0x5EED,
            units: vec![a, b],
        };
        let mut board = SightingsBoard::new(3, 400.0);
        board.refresh(
            OpenField,
            0,
            &[
                crate::ai::PerceptUnit {
                    id: 1,
                    team: 1,
                    pos: [0.0, 1.0, 0.0],
                    sight_range_m: 300.0,
                    radar_scale: 1.0,
                },
                crate::ai::PerceptUnit {
                    id: 5,
                    team: 2,
                    pos: [0.0, 1.0, 60.0],
                    sight_range_m: 300.0,
                    radar_scale: 1.0,
                },
            ],
            &[
                crate::ai::PerceptEnemy {
                    id: 1,
                    team: 1,
                    pos: [0.0, 1.0, 0.0],
                    vel: [0.0; 2],
                    dps: 60.0,
                    band: [0.0, 150.0],
                    lit: false,
                },
                crate::ai::PerceptEnemy {
                    id: 5,
                    team: 2,
                    pos: [0.0, 1.0, 60.0],
                    vel: [0.0; 2],
                    dps: 60.0,
                    band: [0.0, 150.0],
                    lit: false,
                },
            ],
            &[crate::ai::PerceptionParams::default(); 3],
        );
        let mut decisions = BTreeMap::new();
        for id in [1, 5] {
            decisions.insert(
                id,
                Decision {
                    current: Some(OptionId::Push),
                    ..Decision::default()
                },
            );
        }
        let coord = CoordState::default();
        let outs = run(&world, &board, &coord, OpenField, &decisions);
        assert_eq!(outs.len(), 2);
        for (unit, out) in world.units.iter().zip(&outs) {
            let mut rng = AiRng::tick_stream(world.world_seed, world.tick, unit.id);
            let one = unit_output(
                unit,
                &world,
                &board,
                &coord,
                OpenField,
                Some(OptionId::Push),
                &mut rng,
            );
            assert_eq!(*out, one);
            assert!(out.fire.is_some(), "unit {} sees and shoots its foe", unit.id);
        }
        // No decision: no output.
        let outs = run(&world, &board, &coord, OpenField, &BTreeMap::new());
        assert!(outs.iter().all(|o| *o == AiUnitOutput::default()));
    }

    /// Not in DO27: a team past the coordinator's slots has no focus and setting one is ignored; more slots are a
    /// setting.
    #[test]
    fn the_team_count_is_a_setting() {
        let mut c = CoordState::default();
        c.set_focus(7, Some(3));
        assert_eq!(c.focus_id(7), None);
        let mut c = CoordState::with_teams(8);
        c.set_focus(7, Some(3));
        assert_eq!(c.focus_id(7), Some(3));
    }
}
