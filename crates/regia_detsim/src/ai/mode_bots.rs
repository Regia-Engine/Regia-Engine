// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/ai/mode_bots.rs, commit c9513623f, tag regia-absorb-4), ADR-8012.
// Mark: adapt (lines 1-1097, plan row S-13, ADR-8554). The objective view, the KOTH, CTF and FFA/arena goal orders,
// the difficulty knobs and the scripted 2v2 sim are DO27's code apart from rustfmt, doc comments and these named
// changes:
// - the hooks into the match rules move to the rules side: DO27's `mode_kind(MatchMode, ..)`,
//   `Objectives::from_state` and `bots_from_state` read the game's runtime state; here they are
//   `regia_framework::rules::bots` (`bot_mode`, `BotHooks::objectives`, `BotHooks::bodies`) over the S-14 objective
//   board, because this crate sits below the rules. `Objectives` and `BotBody` stay the interface the orders read;
// - the order phase of `ScriptedSim::step` is the public `mode_order` (one bot) and `mode_orders` (every living bot
//   of a snapshot), which the hooks call; `ScriptedSim::step` calls `mode_order` with DO27's own ally and contact
//   lists, so its bits are unchanged;
// - `acquire_range_m` reads S-12's `combat::acquire_range` (DO27's `pilots::acquire_range`);
// - std float `max` and `clamp` go through `regia_detsim::float::det_*` (ADR-8085);
// - DO27's spec-file references and game identifiers are dropped from the docs.
// - ADR-8655 (review of S-13): the orders no longer follow the slice order of their inputs. `nearest_hostile` runs
//   DO27's 1 cm band scan over the contacts in canonical order (id, then position, then team) and returns the contact
//   itself; the FFA ally keep-out sums in canonical order; the pickup run breaks equal distances on the position; the
//   CTF escort's carrier is the canonical first. On id-ordered input every pick is DO27's, bit for bit.

//! Mode-objective bots: free-for-all, arena deathmatch, king of the hill, capture the flag.
//!
//! The mode rules live elsewhere (`regia_framework::rules`); this module does not reimplement scoring, pickup or
//! capture rules. It defines one small objective interface ([`Objectives`], [`BotBody`], [`EnemySight`],
//! [`Pickup`]) and pure per-bot goal orders against it:
//!
//! - KOTH ([`koth_order`]): contest the hill, hold a slot while healthy, rotate out when hurt.
//! - CTF ([`assign_ctf_roles`], [`ctf_order`]): split attack / defend / escort, carry the enemy flag home, return the
//!   dropped own flag.
//! - FFA and arena deathmatch ([`ffa_order`]): hunt the nearest target with lateral spacing, run pickups when nothing
//!   is in range.
//! - [`mode_order`] and [`mode_orders`]: the order of one bot, or of every bot, for a [`ModeKind`].
//!
//! Difficulty reuses the existing knobs only: [`aim_for`] / [`hit_chance`] (aim error), [`reaction_ticks`]
//! (reaction delay), [`acquire_range_m`] (perception reach), [`retreat_hull_pct`] (rotate-out threshold) and
//! [`pickup_radius_m`] (pickup reach). Hull, damage and speed never change with level.
//!
//! Determinism: every function reads its slices as sets: contacts, allies and pickups are scanned in a canonical
//! order (entity id, then position; pickups by distance, then position), so two peers that hold the same contacts
//! in a different slice order give the same orders (ADR-8655). Entity-id order is still the cheap path (no copy).
//! The only randomness is [`AiRng`] mixed from `(world_seed, bot_id)`. No wall clock, no `HashMap` iteration on
//! any decision path.
//! [`ScriptedSim`] is a headless 2v2 harness over the orders.

use super::AiRng;
use super::combat::acquire_range;
use super::difficulty::{AiLevel, AimParams, DifficultyProfile};
use crate::float::{det_clamp, det_cmp, det_max};

/// Which bot behavior set drives a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeKind {
    /// Every unit for itself (no teams).
    Ffa,
    /// Team kill-race on an arena floor.
    Arena,
    /// Hold the hill volume.
    Koth,
    /// Carry and return flags.
    Ctf,
}

/// One bot as the mode layer sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BotBody {
    /// Entity id.
    pub id: u32,
    /// Team (1 and up; 0 is neutral).
    pub team: u8,
    /// World position.
    pub pos: [f32; 3],
    /// 0..1 hull fraction.
    pub hull_pct: f32,
    /// Difficulty level.
    pub level: AiLevel,
    /// True while carrying the enemy flag (CTF escort anchor).
    pub carrying_flag: bool,
}

/// One known contact. In FFA every other bot is hostile; in team arena the
/// caller still passes every contact and the team tag filters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnemySight {
    /// Entity id.
    pub id: u32,
    /// Team.
    pub team: u8,
    /// World position.
    pub pos: [f32; 3],
}

/// One pickup (resource, health, ammo) on the arena floor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pickup {
    /// World position.
    pub pos: [f32; 3],
}

/// One flag as the bot layer sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlagView {
    /// The flag's team.
    pub team: u8,
    /// Home base.
    pub base: [f32; 3],
    /// Live world position (base, dropped point, or carrier position).
    pub pos: [f32; 3],
    /// The carrier's entity id, if carried.
    pub carrier: Option<u32>,
    /// True while idle at its base.
    pub at_base: bool,
}

/// The hill as the bot layer sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HillView {
    /// The hill's center.
    pub center: [f32; 3],
    /// Minimum corner of the hill volume.
    pub min: [f32; 3],
    /// Maximum corner of the hill volume.
    pub max: [f32; 3],
}

impl HillView {
    /// True when `pos` is inside the hill volume (bounds inclusive).
    pub fn contains(&self, pos: [f32; 3]) -> bool {
        pos[0] >= self.min[0]
            && pos[0] <= self.max[0]
            && pos[1] >= self.min[1]
            && pos[1] <= self.max[1]
            && pos[2] >= self.min[2]
            && pos[2] <= self.max[2]
    }
}

/// The small mode-objective interface all four behaviors read. Flag order is
/// by team (lowest first); bot and enemy slices passed to the orders must be
/// in entity-id order.
#[derive(Debug, Clone, PartialEq)]
pub struct Objectives {
    /// The hill, if one is installed.
    pub hill: Option<HillView>,
    /// Sorted by team, lowest first.
    pub flags: Vec<FlagView>,
    /// The sim tick.
    pub tick: u64,
    /// The match seed.
    pub world_seed: u64,
}

impl Objectives {
    /// No hill and no flags (free-for-all and arena).
    pub fn empty(tick: u64, world_seed: u64) -> Self {
        Objectives {
            hill: None,
            flags: Vec::new(),
            tick,
            world_seed,
        }
    }

    /// Enemy flag for `team`: the lowest-team flag not owned by `team`.
    /// `None` in FFA/arena (no flags).
    pub fn enemy_flag(&self, team: u8) -> Option<&FlagView> {
        self.flags.iter().find(|f| f.team != team)
    }

    /// Own flag for `team`.
    pub fn own_flag(&self, team: u8) -> Option<&FlagView> {
        self.flags.iter().find(|f| f.team == team)
    }
}

// ---------------------------------------------------------------------------
// Difficulty knobs (reused, never redefined)
// ---------------------------------------------------------------------------

/// Aim error knobs for a level (lead scale, tracking lag, spread).
pub fn aim_for(level: AiLevel) -> AimParams {
    DifficultyProfile::for_level(level).aim
}

/// Hit-chance helper for scripted sims: wider spread misses more.
/// Pure and deterministic; damage values are untouched.
pub fn hit_chance(spread_mrad: f32, rng: &mut AiRng) -> bool {
    let p = det_clamp(1.0 - spread_mrad / 50.0, 0.05, 0.95);
    rng.next_f32() < p
}

/// Reaction delay in ticks before a fresh contact is engaged.
pub fn reaction_ticks(level: AiLevel) -> u32 {
    DifficultyProfile::for_level(level).reaction_delay_ticks
}

/// Perception reach in metres.
pub fn acquire_range_m(level: AiLevel) -> f32 {
    acquire_range(level)
}

/// Hull fraction below which a KOTH holder rotates out (the same knee the
/// tactic layer uses for retreat).
pub fn retreat_hull_pct(level: AiLevel) -> f32 {
    DifficultyProfile::for_level(level).thresholds.retreat_hull_pct
}

/// Pickup search radius in metres (the economy pace knob).
pub fn pickup_radius_m(level: AiLevel) -> f32 {
    DifficultyProfile::for_level(level).scrap.search_radius_m
}

// ---------------------------------------------------------------------------
// Shared geometry (deterministic, `crate::math` only)
// ---------------------------------------------------------------------------

#[inline]
fn dist_xz(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dz = a[2] - b[2];
    crate::math::sqrt(dx * dx + dz * dz)
}

/// The XZ-then-Y position order: the stable key after the entity id (or, for a pickup, the only key after
/// distance). [`det_cmp`] per axis, so it is a total order with no zero-sign or NaN-payload residue.
fn cmp_pos(a: [f32; 3], b: [f32; 3]) -> core::cmp::Ordering {
    det_cmp(a[0], b[0])
        .then_with(|| det_cmp(a[2], b[2]))
        .then_with(|| det_cmp(a[1], b[1]))
}

/// The canonical contact order every scan below reads: entity id, then position, then team (ADR-8655).
fn cmp_contact(a: &EnemySight, b: &EnemySight) -> core::cmp::Ordering {
    a.id.cmp(&b.id)
        .then_with(|| cmp_pos(a.pos, b.pos))
        .then_with(|| a.team.cmp(&b.team))
}

/// The canonical body order: entity id, then position (ADR-8655).
fn cmp_body(a: &BotBody, b: &BotBody) -> core::cmp::Ordering {
    a.id.cmp(&b.id).then_with(|| cmp_pos(a.pos, b.pos))
}

/// `items` in canonical order: borrowed as is when already strictly ascending (the documented id-ordered input,
/// no allocation), else a sorted copy. Strictly ascending means no two items compare equal, so the copy and the
/// borrow give the same sequence for the same multiset.
fn canonical_order<T: Copy>(
    items: &[T],
    cmp: fn(&T, &T) -> core::cmp::Ordering,
) -> std::borrow::Cow<'_, [T]> {
    if items.windows(2).all(|w| cmp(&w[0], &w[1]).is_lt()) {
        std::borrow::Cow::Borrowed(items)
    } else {
        let mut sorted = items.to_vec();
        sorted.sort_by(cmp);
        std::borrow::Cow::Owned(sorted)
    }
}

/// Nearest hostile contact inside `reach` (m, XZ): skips self and, in team
/// modes, same-team contacts. `free_for_all` treats every other bot as hostile.
///
/// The scan is DO27's 1 cm tie band (a contact replaces the best when it is more than 1 cm nearer, or within 1 cm
/// and of a lower id). The band alone is not a total order, so the scan always runs over the contacts in canonical
/// order ([`cmp_contact`]: id, then position, then team): the pick depends only on the set of contacts, never on
/// the slice order, and on id-ordered input it is the scan DO27 ran, bit for bit (ADR-8655).
fn nearest_hostile(
    bot: &BotBody,
    enemies: &[EnemySight],
    reach: f32,
    free_for_all: bool,
) -> Option<EnemySight> {
    let mut best: Option<(EnemySight, f32)> = None;
    for e in canonical_order(enemies, cmp_contact).iter() {
        if e.id == bot.id {
            continue;
        }
        if !free_for_all && e.team == bot.team {
            continue;
        }
        let d = dist_xz(bot.pos, e.pos);
        if d > reach {
            continue;
        }
        let better = match best {
            None => true,
            Some((be, bd)) => d < bd - 0.01 || ((d - bd).abs() <= 0.01 && e.id < be.id),
        };
        if better {
            best = Some((*e, d));
        }
    }
    best.map(|(e, _)| e)
}

/// Slot on a ring around `center`: rank `i` of `n` holders, radius `r`.
/// Rank comes from entity-id order, so slots are stable across ticks.
fn ring_slot(center: [f32; 3], i: usize, n: usize, r: f32) -> [f32; 3] {
    // Two statements: `usize::max` on a line that names `f32` reads as a float `max` to `cargo xtask det-scan`.
    let n = n.max(1);
    let n = n as f32;
    let angle = 2.0 * core::f32::consts::PI * (i as f32) / n;
    [
        center[0] + crate::math::cos(angle) * r,
        center[1],
        center[2] + crate::math::sin(angle) * r,
    ]
}

// ---------------------------------------------------------------------------
// KOTH
// ---------------------------------------------------------------------------

/// KOTH holder footprint radius inside the hill (m).
pub const HILL_SLOT_RADIUS_M: f32 = 8.0;
/// How far a rotating bot backs away from the hill center (m).
pub const ROTATE_FALLBACK_M: f32 = 60.0;

/// What a KOTH bot is doing this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KothIntent {
    /// Inside the hill on a hold slot.
    Hold,
    /// Outside the hill, driving to a contest slot.
    Contest,
    /// Hurt: backing out to recover and let a teammate take the slot.
    Rotate,
}

/// One bot's goal order for this tick: where to go, whom to focus, and the mode's own intent or role.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModeOrder {
    /// The goal point.
    pub goal: [f32; 3],
    /// The contact to focus, if any.
    pub focus_enemy: Option<u32>,
    /// The KOTH intent (KOTH orders only).
    pub koth: Option<KothIntent>,
    /// The CTF role (CTF orders only).
    pub ctf: Option<CtfRole>,
}

impl ModeOrder {
    fn bare(goal: [f32; 3]) -> Self {
        ModeOrder {
            goal,
            focus_enemy: None,
            koth: None,
            ctf: None,
        }
    }
}

/// Goal order for one KOTH bot.
///
/// `team_rank` is this bot's index among its living teammates in entity-id
/// order (0-based), `team_count` the teammate count. Hurt means
/// `hull_pct < retreat_hull_pct(level)`: the bot backs outward from the
/// hill along its own radial so it does not cross the holders' slots.
pub fn koth_order(
    bot: &BotBody,
    objectives: &Objectives,
    enemies: &[EnemySight],
    team_rank: usize,
    team_count: usize,
) -> ModeOrder {
    let Some(hill) = &objectives.hill else {
        // No hill installed yet: hold where the bot stands.
        return ModeOrder::bare([bot.pos[0], bot.pos[1], bot.pos[2]]);
    };
    if bot.hull_pct < retreat_hull_pct(bot.level) {
        // Rotate out: back away from the hill along the bot's own radial.
        let dx = bot.pos[0] - hill.center[0];
        let dz = bot.pos[2] - hill.center[2];
        let d = det_max(crate::math::sqrt(dx * dx + dz * dz), 1.0e-3);
        let goal = [
            hill.center[0] + dx / d * (d + ROTATE_FALLBACK_M),
            bot.pos[1],
            hill.center[2] + dz / d * (d + ROTATE_FALLBACK_M),
        ];
        return ModeOrder {
            goal,
            focus_enemy: None,
            koth: Some(KothIntent::Rotate),
            ctf: None,
        };
    }
    let slot = ring_slot(hill.center, team_rank, team_count, HILL_SLOT_RADIUS_M);
    let focus_enemy = nearest_hostile(bot, enemies, acquire_range_m(bot.level), false).map(|e| e.id);
    if hill.contains(bot.pos) {
        ModeOrder {
            goal: slot,
            focus_enemy,
            koth: Some(KothIntent::Hold),
            ctf: None,
        }
    } else {
        ModeOrder {
            goal: slot,
            focus_enemy,
            koth: Some(KothIntent::Contest),
            ctf: None,
        }
    }
}

// ---------------------------------------------------------------------------
// CTF
// ---------------------------------------------------------------------------

/// CTF team role, assigned deterministically by entity-id order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtfRole {
    /// Take the enemy flag and run it home.
    Attack,
    /// Guard the own flag and return it when dropped.
    Defend,
    /// Shadow the carrier (or the attacker before the grab) and screen them.
    Escort,
}

/// Split one team's bots (entity-id order) into attack / defend / escort.
/// Index 0 attacks, 1 defends, 2 escorts, then the cycle repeats, so a 2v2
/// is always one attacker plus one defender and a 3v3 adds the escort.
pub fn assign_ctf_roles(sorted_ids: &[u32]) -> Vec<(u32, CtfRole)> {
    sorted_ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let role = match i % 3 {
                0 => CtfRole::Attack,
                1 => CtfRole::Defend,
                _ => CtfRole::Escort,
            };
            (*id, role)
        })
        .collect()
}

/// Escort standoff behind the carrier, away from the enemy base (m).
pub const ESCORT_STANDOFF_M: f32 = 10.0;

/// Goal order for one CTF bot under its assigned role.
///
/// - Attacker carrying the enemy flag runs to the own base; otherwise to the enemy flag's live position (base or
///   dropped point).
/// - Defender holds the own flag's live position (base, or the dropped point to return it by touch).
/// - Escort shadows the team's carrier, or the team's lowest-id living teammate before anyone has the flag, at
///   [`ESCORT_STANDOFF_M`] on the home side.
pub fn ctf_order(
    bot: &BotBody,
    role: CtfRole,
    objectives: &Objectives,
    team_bots: &[BotBody],
    enemies: &[EnemySight],
) -> ModeOrder {
    let focus_enemy = nearest_hostile(bot, enemies, acquire_range_m(bot.level), false).map(|e| e.id);
    let mut order = ModeOrder::bare(bot.pos);
    order.focus_enemy = focus_enemy;
    order.ctf = Some(role);
    match role {
        CtfRole::Attack => {
            if bot.carrying_flag {
                // Run it home: capture needs the own flag at base, so head
                // for the own base even before knowing its state.
                if let Some(own) = objectives.own_flag(bot.team) {
                    order.goal = own.base;
                }
            } else if let Some(enemy) = objectives.enemy_flag(bot.team) {
                order.goal = enemy.pos;
            }
        }
        CtfRole::Defend => {
            if let Some(own) = objectives.own_flag(bot.team) {
                // Dropped own flag: go touch it (ally touch returns it).
                order.goal = own.pos;
            }
        }
        CtfRole::Escort => {
            // The carrier to screen: prefer the teammate actually carrying,
            // else the lowest-id living teammate (the likely grabber).
            // Of several carriers (one per enemy flag with three teams) the
            // canonical first: on id-ordered `team_bots` that is the first
            // carrier in the slice, as DO27 scanned it (ADR-8655).
            let mut anchor: Option<[f32; 3]> = team_bots
                .iter()
                .filter(|b| b.carrying_flag)
                .min_by(|a, b| cmp_body(a, b))
                .map(|b| b.pos);
            if anchor.is_none() {
                // No carrier yet: shadow the lowest-id living teammate, or
                // the enemy flag side when alone.
                let mut lowest_attacker: Option<&BotBody> = None;
                for b in team_bots {
                    if b.id == bot.id {
                        continue;
                    }
                    match lowest_attacker {
                        None => lowest_attacker = Some(b),
                        Some(cur) if b.id < cur.id => lowest_attacker = Some(b),
                        _ => {}
                    }
                }
                anchor = lowest_attacker
                    .map(|b| b.pos)
                    .or_else(|| objectives.enemy_flag(bot.team).map(|f| f.pos));
            }
            if let Some(pos) = anchor {
                // Stand off on the home side: away from the enemy base. When
                // the carrier is still at the enemy base (the grab tick), that
                // direction is degenerate, so fall back to the
                // enemy-base-to-own-base line.
                let away: [f32; 3] = match objectives.enemy_flag(bot.team) {
                    Some(enemy) => {
                        let dx = pos[0] - enemy.base[0];
                        let dz = pos[2] - enemy.base[2];
                        let d = crate::math::sqrt(dx * dx + dz * dz);
                        if d > 1.0 {
                            [dx / d, 0.0, dz / d]
                        } else if let Some(own) = objectives.own_flag(bot.team) {
                            let ox = own.base[0] - enemy.base[0];
                            let oz = own.base[2] - enemy.base[2];
                            let od = det_max(crate::math::sqrt(ox * ox + oz * oz), 1.0e-3);
                            [ox / od, 0.0, oz / od]
                        } else {
                            [0.0, 0.0, 1.0]
                        }
                    }
                    None => [0.0, 0.0, 1.0],
                };
                order.goal = [
                    pos[0] + away[0] * ESCORT_STANDOFF_M,
                    pos[1],
                    pos[2] + away[2] * ESCORT_STANDOFF_M,
                ];
            }
        }
    }
    order
}

// ---------------------------------------------------------------------------
// FFA / arena deathmatch
// ---------------------------------------------------------------------------

/// Lateral spacing between hunters on the same target (m).
pub const HUNT_SPACING_M: f32 = 6.0;
/// Allies closer than this (m, XZ) push a hunter's goal sideways.
pub const HUNT_KEEPOUT_M: f32 = 4.0;

/// Goal order for one FFA / arena bot: hunt the nearest enemy in acquire
/// range with a per-id lateral offset so two hunters do not stack, else run
/// the nearest pickup inside the difficulty's search radius, else hold.
pub fn ffa_order(
    bot: &BotBody,
    allies: &[BotBody],
    enemies: &[EnemySight],
    pickups: &[Pickup],
    free_for_all: bool,
) -> ModeOrder {
    let reach = acquire_range_m(bot.level);
    // The contact itself, not a lookup by id: two contacts with one id (a stale and a fresh sighting) would make
    // a `find` by id read the slice order.
    let hunted: Option<EnemySight> = nearest_hostile(bot, enemies, reach, free_for_all);
    if let Some(target) = hunted {
        // Lateral offset by id parity so paired hunters flank, not stack.
        let side = if bot.id.is_multiple_of(2) { 1.0 } else { -1.0 };
        let dx = target.pos[0] - bot.pos[0];
        let dz = target.pos[2] - bot.pos[2];
        let d = det_max(crate::math::sqrt(dx * dx + dz * dz), 1.0e-3);
        // Perpendicular: (-dz, dx) / d, scaled by spacing.
        let mut goal = [
            target.pos[0] + (-dz / d) * side * HUNT_SPACING_M,
            target.pos[1],
            target.pos[2] + (dx / d) * side * HUNT_SPACING_M,
        ];
        // Ally keep-out: nudge sideways when a teammate is on top of us. The pushes are summed in canonical
        // order (id, then position): float addition is not associative, so the sum must not follow the slice.
        for ally in canonical_order(allies, cmp_body).iter() {
            if ally.id == bot.id {
                continue;
            }
            let ad = dist_xz(bot.pos, ally.pos);
            if ad < HUNT_KEEPOUT_M {
                let ax = bot.pos[0] - ally.pos[0];
                let az = bot.pos[2] - ally.pos[2];
                let al = det_max(crate::math::sqrt(ax * ax + az * az), 1.0e-3);
                let push = HUNT_KEEPOUT_M - ad;
                goal[0] += ax / al * push;
                goal[2] += az / al * push;
            }
        }
        return ModeOrder {
            goal,
            focus_enemy: Some(target.id),
            koth: None,
            ctf: None,
        };
    }
    // No target in range: pickup run inside the difficulty search radius. Nearest first, then the position order
    // ([`cmp_pos`]): a total order, so two pickups at one distance pick the same one in any slice order.
    let radius = pickup_radius_m(bot.level);
    let mut best_pick: Option<([f32; 3], f32)> = None;
    for p in pickups {
        let d = dist_xz(bot.pos, p.pos);
        if d > radius {
            continue;
        }
        let better = match best_pick {
            None => true,
            Some((bp, bd)) => det_cmp(d, bd).then_with(|| cmp_pos(p.pos, bp)).is_lt(),
        };
        if better {
            best_pick = Some((p.pos, d));
        }
    }
    if let Some((pos, _)) = best_pick {
        return ModeOrder::bare(pos);
    }
    ModeOrder::bare(bot.pos)
}

// ---------------------------------------------------------------------------
// The order of one bot, and of every bot
// ---------------------------------------------------------------------------

/// The goal order of one living `bot` under `mode`. `allies` are its living teammates (itself excluded) and
/// `enemies` every contact it knows (itself and teammates may be included: the orders filter them), both in
/// entity-id order. KOTH ranks the bot among its team by id, CTF assigns the team's roles by id
/// ([`assign_ctf_roles`]), FFA and arena hunt ([`ffa_order`]; `pickups` are only read there).
pub fn mode_order(
    mode: ModeKind,
    bot: &BotBody,
    allies: &[BotBody],
    enemies: &[EnemySight],
    objectives: &Objectives,
    pickups: &[Pickup],
) -> ModeOrder {
    match mode {
        ModeKind::Koth => {
            let mut ids: Vec<u32> = allies
                .iter()
                .map(|b| b.id)
                .chain(core::iter::once(bot.id))
                .collect();
            ids.sort();
            let rank = ids.iter().position(|id| *id == bot.id).unwrap_or(0);
            koth_order(bot, objectives, enemies, rank, ids.len().max(1))
        }
        ModeKind::Ctf => {
            let team_ids: Vec<u32> = {
                let mut ids: Vec<u32> = allies
                    .iter()
                    .map(|b| b.id)
                    .chain(core::iter::once(bot.id))
                    .collect();
                ids.sort();
                ids
            };
            let roles = assign_ctf_roles(&team_ids);
            let role = roles
                .iter()
                .find(|(id, _)| *id == bot.id)
                .map(|(_, r)| *r)
                .unwrap_or(CtfRole::Attack);
            let mut team_bots = allies.to_vec();
            team_bots.push(*bot);
            team_bots.sort_by_key(|b| b.id);
            ctf_order(bot, role, objectives, &team_bots, enemies)
        }
        ModeKind::Ffa | ModeKind::Arena => ffa_order(bot, allies, enemies, pickups, mode == ModeKind::Ffa),
    }
}

/// Every bot's goal order for one snapshot, in entity-id order. `bots` are the living bots (any order; dead ones,
/// `hull_pct <= 0`, get no order); `contacts` are the contacts every bot knows (the hooks pass every living bot, a
/// host with sighting boards calls [`mode_order`] per bot with its team's contacts instead).
pub fn mode_orders(
    mode: ModeKind,
    bots: &[BotBody],
    contacts: &[EnemySight],
    objectives: &Objectives,
    pickups: &[Pickup],
) -> Vec<(u32, ModeOrder)> {
    let mut living: Vec<BotBody> = bots.iter().filter(|b| b.hull_pct > 0.0).copied().collect();
    living.sort_by_key(|b| b.id);
    let mut enemies: Vec<EnemySight> = contacts.to_vec();
    enemies.sort_by_key(|e| e.id);
    living
        .iter()
        .map(|bot| {
            let allies: Vec<BotBody> = living
                .iter()
                .filter(|b| b.id != bot.id && b.team == bot.team)
                .copied()
                .collect();
            (
                bot.id,
                mode_order(mode, bot, &allies, &enemies, objectives, pickups),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Scripted 2v2 sim (deterministic; no wall clock, no hash iteration)
// ---------------------------------------------------------------------------

/// Sim movement per step (m).
pub const SIM_STEP_M: f32 = 2.0;
/// Sim damage per step inside engage range (flat for every level).
pub const SIM_DAMAGE_PER_STEP: f32 = 5.0;
/// Sim engage range (m, XZ).
pub const SIM_ENGAGE_M: f32 = 25.0;

/// One sim bot: position, hull, and first-seen tick per enemy slot.
#[derive(Debug, Clone, PartialEq)]
pub struct SimBot {
    /// The bot's body.
    pub body: BotBody,
    /// Its goal from the last order.
    pub goal: [f32; 3],
    /// Its focus from the last order.
    pub focus: Option<u32>,
    /// First-seen tick per enemy id, in enemy-id order (parallel vec).
    pub seen: Vec<(u32, u64)>,
}

impl SimBot {
    /// A full-hull bot standing at `pos`.
    pub fn new(id: u32, team: u8, pos: [f32; 3], level: AiLevel) -> Self {
        SimBot {
            body: BotBody {
                id,
                team,
                pos,
                hull_pct: 1.0,
                level,
                carrying_flag: false,
            },
            goal: pos,
            focus: None,
            seen: Vec::new(),
        }
    }

    fn note_seen(&mut self, enemy_id: u32, tick: u64) {
        if !self.seen.iter().any(|(id, _)| *id == enemy_id) {
            self.seen.push((enemy_id, tick));
            self.seen.sort_by_key(|(id, _)| *id);
        }
    }

    fn first_seen(&self, enemy_id: u32) -> Option<u64> {
        self.seen.iter().find(|(id, _)| *id == enemy_id).map(|(_, t)| *t)
    }
}

/// Minimal scripted 2v2 world: four bots (ids sorted), shared objectives,
/// seeded RNG per bot per step. One [`ScriptedSim::step`] moves every bot
/// toward its mode goal, then applies flat-damage trades inside
/// [`SIM_ENGAGE_M`] once the shooter's reaction delay has passed and the
/// difficulty's hit chance allows it.
#[derive(Debug, Clone)]
pub struct ScriptedSim {
    /// The bots, in id order.
    pub bots: Vec<SimBot>,
    /// The shared objectives.
    pub objectives: Objectives,
    /// The behavior set.
    pub mode: ModeKind,
    /// The sim tick.
    pub tick: u64,
    /// The match seed.
    pub world_seed: u64,
    /// Steps run so far.
    pub steps_taken: u64,
}

impl ScriptedSim {
    /// 2v2: ids 1-2 on team 1, 3-4 on team 2.
    pub fn two_v_two(
        mode: ModeKind,
        team1_pos: [[f32; 3]; 2],
        team2_pos: [[f32; 3]; 2],
        objectives: Objectives,
        level: AiLevel,
        world_seed: u64,
    ) -> Self {
        let bots = vec![
            SimBot::new(1, 1, team1_pos[0], level),
            SimBot::new(2, 1, team1_pos[1], level),
            SimBot::new(3, 2, team2_pos[0], level),
            SimBot::new(4, 2, team2_pos[1], level),
        ];
        ScriptedSim {
            bots,
            objectives,
            mode,
            tick: 0,
            world_seed,
            steps_taken: 0,
        }
    }

    /// Every other living bot with its team tag, in entity-id order. Each
    /// order filters what it treats as hostile (`nearest_hostile`), so the
    /// sim never pre-filters here.
    fn enemies_of(&self, bot_id: u32) -> Vec<EnemySight> {
        let mut out: Vec<EnemySight> = self
            .bots
            .iter()
            .filter(|b| b.body.id != bot_id && b.body.hull_pct > 0.0)
            .map(|b| EnemySight {
                id: b.body.id,
                team: b.body.team,
                pos: b.body.pos,
            })
            .collect();
        out.sort_by_key(|e| e.id);
        out
    }

    fn allies_of(&self, bot_id: u32, team: u8) -> Vec<BotBody> {
        let mut out: Vec<BotBody> = self
            .bots
            .iter()
            .filter(|b| b.body.id != bot_id && b.body.team == team && b.body.hull_pct > 0.0)
            .map(|b| b.body)
            .collect();
        out.sort_by_key(|b| b.id);
        out
    }

    /// One deterministic step: order every bot, move, then trade damage.
    pub fn step(&mut self) {
        self.tick += 1;
        // Order phase (snapshot reads only; entity-id order).
        let mut orders: Vec<(u32, ModeOrder)> = Vec::with_capacity(self.bots.len());
        for bot in &self.bots {
            if bot.body.hull_pct <= 0.0 {
                continue;
            }
            let enemies = self.enemies_of(bot.body.id);
            let allies = self.allies_of(bot.body.id, bot.body.team);
            let order = mode_order(self.mode, &bot.body, &allies, &enemies, &self.objectives, &[]);
            orders.push((bot.body.id, order));
        }
        orders.sort_by_key(|(id, _)| *id);
        // Move phase.
        for (id, order) in &orders {
            if let Some(bot) = self.bots.iter_mut().find(|b| b.body.id == *id) {
                bot.goal = order.goal;
                bot.focus = order.focus_enemy;
                step_toward(&mut bot.body.pos, order.goal, SIM_STEP_M);
            }
        }
        // Damage phase: flat trades inside SIM_ENGAGE_M after reaction.
        let mut hits: Vec<(u32, f32)> = Vec::new();
        for bot in &self.bots {
            if bot.body.hull_pct <= 0.0 {
                continue;
            }
            let Some(focus) = bot.focus else { continue };
            let enemy_pos = self.bots.iter().find(|b| b.body.id == focus).map(|b| b.body.pos);
            let Some(pos) = enemy_pos else { continue };
            if dist_xz(bot.body.pos, pos) > SIM_ENGAGE_M {
                continue;
            }
            let first = bot.first_seen(focus).unwrap_or(self.tick);
            if self.tick.saturating_sub(first) < reaction_ticks(bot.body.level) as u64 {
                continue;
            }
            let mut rng = AiRng::new(AiRng::mix(
                self.world_seed ^ (self.tick.wrapping_mul(0x9E37_79B9_7F4A_7C15)),
                bot.body.id as u64,
            ));
            if hit_chance(aim_for(bot.body.level).spread_mrad, &mut rng) {
                hits.push((focus, SIM_DAMAGE_PER_STEP));
            }
        }
        hits.sort_by_key(|(id, _)| *id);
        for (id, dmg) in hits {
            if let Some(bot) = self.bots.iter_mut().find(|b| b.body.id == id) {
                bot.body.hull_pct = det_max(bot.body.hull_pct - dmg / 100.0, 0.0);
            }
        }
        // Sight phase: record fresh contacts (entity-id order).
        let tick = self.tick;
        let snapshot: Vec<(u32, u8, [f32; 3], f32)> = self
            .bots
            .iter()
            .map(|b| (b.body.id, b.body.team, b.body.pos, b.body.hull_pct))
            .collect();
        for bot in &mut self.bots {
            if bot.body.hull_pct <= 0.0 {
                continue;
            }
            let reach = acquire_range_m(bot.body.level);
            for (id, team, pos, hull) in &snapshot {
                if *id == bot.body.id || *hull <= 0.0 {
                    continue;
                }
                if self.mode != ModeKind::Ffa && *team == bot.body.team {
                    continue;
                }
                if dist_xz(bot.body.pos, *pos) <= reach {
                    bot.note_seen(*id, tick);
                }
            }
        }
        self.steps_taken += 1;
    }

    /// Run `n` steps.
    pub fn run(&mut self, n: u64) {
        for _ in 0..n {
            self.step();
        }
    }

    /// Quantized trace digest: same seed and mode give the same digest.
    pub fn digest(&self) -> u64 {
        let mut h = self.world_seed;
        h = h.wrapping_mul(0x9E37_79B9).wrapping_add(self.steps_taken);
        for bot in &self.bots {
            let x = (bot.body.pos[0] * 100.0) as i32 as u64;
            let z = (bot.body.pos[2] * 100.0) as i32 as u64;
            let hull = (bot.body.hull_pct * 1000.0) as u64;
            h = h.wrapping_mul(0x9E37_79B9).wrapping_add(x ^ z.rotate_left(7));
            h = h.wrapping_mul(0x9E37_79B9).wrapping_add(hull);
        }
        h
    }
}

fn step_toward(pos: &mut [f32; 3], goal: [f32; 3], step: f32) {
    let dx = goal[0] - pos[0];
    let dz = goal[2] - pos[2];
    let d2 = dx * dx + dz * dz;
    if d2 < step * step {
        pos[0] = goal[0];
        pos[2] = goal[2];
        return;
    }
    let inv = step / crate::math::sqrt(d2);
    pos[0] += dx * inv;
    pos[2] += dz * inv;
}

#[cfg(test)]
mod tests;
