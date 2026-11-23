// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/ai/wingman.rs, commit c9513623f, tag regia-absorb-4), ADR-8012.
// Mark: adapt (lines 1-483 and 506-1319, plan row S-13, ADR-8554; lines 484-505 moved to `combat.rs` with S-12).
// The formation geometry, the level-ground anchor, the command vocabulary, the plan resolution, the reports, the
// repair and ammo runs, group routing and the plan-to-motor drive are DO27's code apart from rustfmt, doc comments and
// these named changes:
// - the orders-to-plans interface: DO27's `from_unit_order` matched the game's own order enum (and its formation
//   kinds); here a host order type implements `WingmanOrder` (DO27's match body moves into DO27's impl, ported as the
//   test fixture of `every_wingman_unit_order_maps_onto_the_vocabulary`), and DO27's team-order enum becomes the
//   neutral `TeamDirective` (`from_team_order` -> `from_team_directive`, same mapping; the two nav-point variants
//   carry no payload because the mapping never read it). The stance code mapping is `stance_from_code`;
// - `CalloutId::cue_stem` named the game's voice lines (content); a callout carries a neutral `name()` and the host
//   maps it to its own lines;
// - the Advance shift reads DO27's order-book lead distance as `ADVANCE_LEAD_M` (12 m, `orders.rs` line 22);
// - line of sight is any S-10 `LineOfSight` (DO27's `Option<&TerrainSampler>`; `OpenField` is DO27's `None`);
// - `LOW_HULL_PCT`, `LOW_AMMO_PCT` and `ammo_pct` live in `combat` (S-12) and are read from there;
// - DO27's wingman override inside its game pipeline (ir-sim/src/ai/mod.rs lines 1288-1300 and 1431-1459, step 4b)
//   is `apply_commands` and `run_commanded` over the S-12 combat run: the commanded unit's option is committed with
//   `commit_bias` (DO27's four field writes) and its output replaced by `drive_from`, in world order, on the same
//   per-unit-per-tick stream;
// - std float `max` and `clamp` go through `regia_detsim::float::det_*` (ADR-8085); `x % 2 == 0` is written
//   `is_multiple_of(2)`; `parse` and the public items gain doc comments;
// - DO27's game identifiers (hull, weapon and building names, the design-doc section numbers) are dropped from
//   docs and tests.

//! Wingman commands, formations, stance and callouts: the good-teammate layer over the tactics, motor and combat
//! layers.
//!
//! It turns a commander's order into a per-unit *plan* ([`WingmanPlan`]): a steer goal, a fire gate, a callout and
//! an option bias for the utility scorer. It owns the four formation shapes ([`WingmanFormation`], [`wingman_slot`]),
//! the "keep out of the player's line of fire" rule ([`slot_clears_leader_lane`], [`fire_blocked`]), the low-hull and
//! low-ammo reports ([`report`]) and the repair and ammo runs ([`repair_goal`], [`ammo_goal`]).
//!
//! The orders-to-plans interface: a host order type implements [`WingmanOrder`] (or the host builds a
//! [`WingmanCommand`] directly); [`plan`] resolves it for one unit and [`route`] for a group, slotting members by
//! sorted entity id. [`drive`] hands a plan to the motor ([`super::arrive_moving`]) and the fire seam
//! ([`super::fire_intent`]), with the allied keep-out and the Re-up override in front.
//!
//! Pure and deterministic: every function reads only the [`AiWorld`] snapshot and its arguments, never writes the
//! world, and iterates in entity-id order.

use std::collections::BTreeMap;

use super::AiRng;
use super::aim::Stance;
use super::combat::{
    LOW_AMMO_PCT, LOW_HULL_PCT, TeamTargets, ammo_pct, escort_goal, fire_intent_from, reup_needed,
    reup_point, run_from as combat_run_from, separate_from_allies,
};
use super::influence::line_of_fire;
use super::los::LineOfSight;
use super::motor::arrive_moving;
use super::perception::SightingsBoard;
use super::tactics::Decision;
use super::utility::OptionId;
use super::view::{AiUnitOutput, AiUnitView, AiWorld, CoordState};
use crate::float::{det_cmp, det_max};

// ---------------------------------------------------------------------------
// Formation geometry
// ---------------------------------------------------------------------------

/// Authored gap between adjacent wingmen in a Line or Wedge (metres). The
/// ±20% spacing acceptance is measured against this and
/// [`WINGMAN_COLUMN_GAP_M`].
pub const WINGMAN_SPACING_M: f32 = 12.0;
/// Follow-close halves the slot spacing.
pub const WINGMAN_CLOSE_SPACING_M: f32 = 6.0;
/// Gap between files in a Column or the back-step of a Wedge.
pub const WINGMAN_COLUMN_GAP_M: f32 = 16.0;
/// Scatter radius of a Loose formation.
pub const WINGMAN_LOOSE_RADIUS_M: f32 = 22.0;
/// How far a wingman parks from its slot before it counts as "arrived".
pub const WINGMAN_ARRIVE_M: f32 = 2.5;
/// The forward lane ahead of the leader stays clear (the player's fire cone):
/// no formation slot sits closer to dead-ahead than this lateral distance.
pub const WINGMAN_LANE_CLEAR_M: f32 = 14.0;
/// How far a Retreat plan steps away from the nearest threat.
pub const WINGMAN_RETREAT_STEP_M: f32 = 40.0;
/// How far ahead of the anchor an Advance screen leads (the order book's lead distance), before one more spacing.
pub const ADVANCE_LEAD_M: f32 = 12.0;

/// The four formation shapes of the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WingmanFormation {
    /// Abreast line beside and behind the leader, one spacing back.
    Line,
    /// A V trailing the leader: files step back, wings out at the lane edge.
    Wedge,
    /// Single file behind the leader.
    Column,
    /// Scattered ring, no exact spacing (the acceptance does not apply).
    Loose,
}

impl WingmanFormation {
    /// Parse a formation name (`line`, `wedge`, `column`, `loose`, each also with a `form_` prefix).
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "line" | "form_line" => Some(Self::Line),
            "wedge" | "form_wedge" => Some(Self::Wedge),
            "column" | "form_column" => Some(Self::Column),
            "loose" | "form_loose" => Some(Self::Loose),
            _ => None,
        }
    }
}

/// Whether a formation is authored with exact spacing (the ±20% acceptance
/// applies to Line, Wedge and Column; Loose is deliberately scattered).
pub fn formation_is_spaced(formation: WingmanFormation) -> bool {
    !matches!(formation, WingmanFormation::Loose)
}

/// The world slot for wingman `index` (0-based) of `formation`, built on the
/// leader's pose. `close` halves the spacing for a follow-close slot.
///
/// The leader frame: forward = `(sin yaw, cos yaw)` in x/z, right = `(cos yaw, -sin yaw)`. Every slot is
/// authored **behind** the leader (forward offset < 0) so the leader's fire
/// lane stays clear; Wedge wings stay at or beyond [`WINGMAN_LANE_CLEAR_M`].
pub fn wingman_slot(
    formation: WingmanFormation,
    index: usize,
    anchor: [f32; 3],
    yaw: f32,
    close: bool,
) -> [f32; 3] {
    let gap = if close {
        WINGMAN_CLOSE_SPACING_M
    } else {
        WINGMAN_SPACING_M
    };
    let (s, c) = (crate::math::sin(yaw), crate::math::cos(yaw));
    // Forward unit vector and right unit vector in the leader frame.
    let (fx, fz) = (s, c);
    let (rx, rz) = (c, -s);

    let (right, forward) = match formation {
        WingmanFormation::Column => (0.0, -((index + 1) as f32) * WINGMAN_COLUMN_GAP_M),
        WingmanFormation::Line => {
            // Adjacent slots are exactly `gap` apart; the line centres on the
            // leader (index 0 sits directly behind), one spacing back.
            let lateral = index as f32 * gap - gap * 0.5;
            (lateral, -gap)
        }
        WingmanFormation::Wedge => {
            let side = if index.is_multiple_of(2) { 1.0 } else { -1.0 };
            let file = (index / 2) as f32;
            (
                side * WINGMAN_LANE_CLEAR_M,
                -((file + 1.0) * WINGMAN_COLUMN_GAP_M + gap),
            )
        }
        WingmanFormation::Loose => {
            // Deterministic scatter behind the leader, offset by index.
            let angle = (index + 1) as f32 * 2.399_963; // golden-angle steps
            let rad = WINGMAN_LOOSE_RADIUS_M * (0.5 + 0.25 * ((index % 3) as f32));
            (
                crate::math::sin(angle) * rad,
                -WINGMAN_LOOSE_RADIUS_M * 0.5 - crate::math::cos(angle) * rad * 0.4,
            )
        }
    };

    [
        anchor[0] + fx * forward + rx * right,
        anchor[1],
        anchor[2] + fz * forward + rz * right,
    ]
}

/// All slots for `count` wingmen of a formation, in router order.
pub fn formation_slots(
    formation: WingmanFormation,
    count: usize,
    anchor: [f32; 3],
    yaw: f32,
    close: bool,
) -> Vec<[f32; 3]> {
    (0..count)
        .map(|i| wingman_slot(formation, i, anchor, yaw, close))
        .collect()
}

/// Group routing: assign every unit id a formation slot. Deterministic — ids
/// are sorted before slotting, so the same command on the same ids always
/// produces the same slots regardless of caller order (network-safe).
pub fn assign_formation_slots(
    unit_ids: &[u32],
    formation: WingmanFormation,
    anchor: [f32; 3],
    yaw: f32,
    close: bool,
) -> BTreeMap<u32, [f32; 3]> {
    let mut ids: Vec<u32> = unit_ids.to_vec();
    ids.sort_unstable();
    ids.into_iter()
        .enumerate()
        .map(|(index, id)| (id, wingman_slot(formation, index, anchor, yaw, close)))
        .collect()
}

/// The most any slot's ground may differ from the anchor's ground (metres). A wedge laid across a hillside puts
/// one file at the foot and one on the plateau: the units path to different levels and never close to the authored
/// spacing.
pub const WINGMAN_LEVEL_M: f32 = 4.0;
/// Search step of the level-ground anchor search (metres).
pub const WINGMAN_LEVEL_STEP_M: f32 = 4.0;
/// Search reach of the level-ground anchor search (metres).
pub const WINGMAN_LEVEL_REACH_M: f32 = 64.0;
/// Directions tried on every search ring (a full turn).
const WINGMAN_LEVEL_DIRS: usize = 16;
/// Most slots checked for level ground (the formation plan never routes more than this many wings to one shape).
const WINGMAN_LEVEL_MAX_SLOTS: usize = 6;

/// True when the `count` slots of `formation` built on `anchor` all stand within [`WINGMAN_LEVEL_M`] of the anchor's
/// ground.
fn formation_is_level(
    formation: WingmanFormation,
    count: usize,
    anchor: [f32; 3],
    yaw: f32,
    close: bool,
    ground: &dyn Fn(f32, f32) -> f32,
) -> bool {
    let base = ground(anchor[0], anchor[2]);
    (0..count.clamp(1, WINGMAN_LEVEL_MAX_SLOTS)).all(|i| {
        let slot = wingman_slot(formation, i, anchor, yaw, close);
        (ground(slot[0], slot[2]) - base).abs() <= WINGMAN_LEVEL_M
    })
}

/// The anchor a formation is laid out on. Normally the leader's own position. When any slot would stand on a
/// different level (the leader is on a steep face, a ledge or a ramp, so a file lands on the plateau or in the pit),
/// the anchor is moved to the nearest spot, searched in rings round the leader, where the whole shape stands on level
/// ground. The slots keep the authored spacing there. The ring search is fixed-order, so the choice is deterministic.
/// Open ground takes the fast path and returns the leader's position unchanged; with nothing level in reach the
/// leader's position is returned too. `ground(x, z)` is the host's ground height.
pub fn level_formation_anchor(
    formation: WingmanFormation,
    count: usize,
    anchor: [f32; 3],
    yaw: f32,
    close: bool,
    ground: &dyn Fn(f32, f32) -> f32,
) -> [f32; 3] {
    if formation_is_level(formation, count, anchor, yaw, close, ground) {
        return anchor;
    }
    let hover = anchor[1] - ground(anchor[0], anchor[2]);
    let rings = (WINGMAN_LEVEL_REACH_M / WINGMAN_LEVEL_STEP_M) as usize;
    for ring in 1..=rings {
        let r = ring as f32 * WINGMAN_LEVEL_STEP_M;
        for k in 0..WINGMAN_LEVEL_DIRS {
            let a = k as f32 * (core::f32::consts::TAU / WINGMAN_LEVEL_DIRS as f32);
            let (x, z) = (
                anchor[0] + crate::math::sin(a) * r,
                anchor[2] + crate::math::cos(a) * r,
            );
            let probe = [x, ground(x, z) + hover, z];
            if formation_is_level(formation, count, probe, yaw, close, ground) {
                return probe;
            }
        }
    }
    anchor
}

/// Signed spacing error of two slot points against an authored gap, as a
/// fraction (`0` = exact). The acceptance window is ±0.20.
pub fn spacing_error(a: [f32; 3], b: [f32; 3], authored: f32) -> f32 {
    if authored <= 0.0 {
        return 0.0;
    }
    (dist_xz(a, b) - authored) / authored
}

/// True when a formation slot sits out of the leader's forward fire lane:
/// either behind the leader, or laterally clear of the cone.
pub fn slot_clears_leader_lane(slot: [f32; 3], anchor: [f32; 3], yaw: f32) -> bool {
    let dx = slot[0] - anchor[0];
    let dz = slot[2] - anchor[2];
    let (s, c) = (crate::math::sin(yaw), crate::math::cos(yaw));
    let forward = dx * s + dz * c;
    let right = dx * c - dz * s;
    forward <= 0.0 || right.abs() >= WINGMAN_LANE_CLEAR_M
}

// ---------------------------------------------------------------------------
// The command vocabulary and the orders-to-plans interface
// ---------------------------------------------------------------------------

/// Every wingman command. This is the vocabulary a commander's HUD emits and the sim's command path routes to
/// [`plan`]; it is not a wire type (a host maps its own orders onto it through [`WingmanOrder`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WingmanCommand {
    /// Follow / form up, with a formation choice and an optional close flag.
    Follow {
        /// The leader's entity id.
        leader: u32,
        /// The formation shape.
        formation: WingmanFormation,
        /// Follow close (half spacing).
        close: bool,
    },
    /// Forward screen: an abreast line ahead of the anchor, pushing toward the enemy while staying spaced.
    Advance {
        /// The anchor's entity id.
        leader: u32,
    },
    /// Flank: abreast of the anchor, off to the side at the lane edge, closing on the enemy's side.
    Flank {
        /// The anchor's entity id.
        leader: u32,
    },
    /// Attack my target.
    AttackTarget {
        /// The target's entity id.
        target: u32,
    },
    /// Attack at will (clear the standing order, hand back to the scorer).
    AttackAtWill,
    /// Defend me / escort (escort-as-follow; stay out of my forward arc).
    Escort {
        /// The leader's entity id.
        leader: u32,
    },
    /// Escort the team's commander: the broadcast Follow ([`TeamDirective::Follow`]), which names no leader.
    /// [`plan`] resolves the leader from the world each tick: the allied commander hull (`is_commander`), else the
    /// lowest-id allied hull (the leader escorts form on in the combat layer), then plans exactly as
    /// [`WingmanCommand::Escort`] on it. A commander, or a unit with no allied hull, holds where it stands.
    EscortCommander,
    /// Hold position / guard here (hold the point where the order landed; the origin means "where I stand").
    Hold {
        /// The point to hold.
        point: [f32; 3],
    },
    /// Defend a point (already resolved to world coordinates).
    DefendPoint {
        /// The point to defend.
        point: [f32; 3],
    },
    /// Defend a base or building by entity id (resolved to a point in [`plan`]).
    DefendEntity {
        /// The entity to defend.
        entity: u32,
    },
    /// Patrol between two points.
    Patrol {
        /// The first waypoint.
        a: [f32; 3],
        /// The second waypoint.
        b: [f32; 3],
        /// Heading for `b` (else `a`).
        toward_b: bool,
    },
    /// Gather resources (the host's economy owns the routing).
    CollectScrap,
    /// Regroup / return to me.
    Regroup {
        /// The leader's entity id.
        leader: u32,
    },
    /// Retreat / disengage.
    Retreat,
    /// Pick up / rescue crew on foot.
    PickupPilot {
        /// The crew member's entity id.
        pilot: u32,
    },
    /// Cease fire.
    CeaseFire,
    /// Weapons free.
    WeaponsFree,
    /// Stance (aggressive / defensive / hold fire).
    Stance(Stance),
}

/// A host's own order type, mapped onto the wingman vocabulary: the orders-to-plans interface. Return `None` for
/// orders the host's sim steers by itself (they never reach the wingman layer).
pub trait WingmanOrder {
    /// The wingman command this order resolves to, if any.
    fn wingman_command(&self) -> Option<WingmanCommand>;
}

impl WingmanOrder for WingmanCommand {
    fn wingman_command(&self) -> Option<WingmanCommand> {
        Some(*self)
    }
}

/// A broadcast (team-level) order, for units with no per-unit order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamDirective {
    /// Attack.
    Attack,
    /// Follow the commander.
    Follow,
    /// Defend where you are.
    Defend,
    /// Retreat.
    Retreat,
    /// Move to a navigation point (the host steers the move).
    GoTo,
    /// Attack toward a navigation point (the host steers the move).
    AttackAt,
}

/// A fire-stance code (`0` aggressive, `1` defensive, anything else hold fire) as a [`Stance`].
pub fn stance_from_code(code: u8) -> Stance {
    match code {
        0 => Stance::Aggressive,
        1 => Stance::Defensive,
        _ => Stance::HoldFire,
    }
}

impl WingmanCommand {
    /// Any host order through the orders-to-plans interface ([`WingmanOrder`]).
    pub fn from_order(order: &impl WingmanOrder) -> Option<Self> {
        order.wingman_command()
    }

    /// The broadcast (team-level) order, used when a unit has no per-unit order.
    pub fn from_team_directive(order: TeamDirective) -> Self {
        match order {
            TeamDirective::Attack => Self::AttackAtWill,
            TeamDirective::Follow => Self::EscortCommander,
            TeamDirective::Defend => Self::Hold {
                point: [0.0, 0.0, 0.0],
            },
            TeamDirective::Retreat => Self::Retreat,
            TeamDirective::GoTo | TeamDirective::AttackAt => Self::AttackAtWill,
        }
    }

    /// The utility option this command biases. `None` means "hand the choice back to the scorer".
    pub fn option_bias(self) -> Option<OptionId> {
        match self {
            Self::Follow { .. } => Some(OptionId::Regroup),
            Self::Advance { .. } => Some(OptionId::Push),
            Self::Flank { .. } => Some(OptionId::Flank),
            Self::AttackTarget { .. } => Some(OptionId::FocusFire),
            Self::AttackAtWill => None,
            Self::Escort { .. } | Self::EscortCommander => Some(OptionId::Escort),
            Self::Hold { .. } => Some(OptionId::Hold),
            Self::DefendPoint { .. } | Self::DefendEntity { .. } => Some(OptionId::Defend),
            Self::Patrol { .. } => None,
            Self::CollectScrap => Some(OptionId::CollectScrap),
            Self::Regroup { .. } => Some(OptionId::Regroup),
            Self::Retreat => Some(OptionId::Retreat),
            Self::PickupPilot { .. } => None,
            Self::CeaseFire | Self::WeaponsFree => None,
            Self::Stance(stance) => match stance {
                Stance::Aggressive => Some(OptionId::Push),
                Stance::Defensive => Some(OptionId::Hold),
                Stance::HoldFire => None,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// The plan (behavior output)
// ---------------------------------------------------------------------------

/// Where a wingman steers: a world point and a park radius.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SteerGoal {
    /// The goal point.
    pub point: [f32; 3],
    /// Park radius, metres.
    pub arrive_m: f32,
    /// Velocity of the anchor this goal is attached to (a moving leader's
    /// formation slot). [`drive`] adds it as a feedforward so a wingman matches
    /// the leader's speed instead of stop-and-go. `[0.0; 3]` = a stationary
    /// goal (hold / defend / patrol / retreat points).
    pub anchor_vel: [f32; 3],
}

impl SteerGoal {
    /// A goal that stays put.
    pub fn still(point: [f32; 3], arrive_m: f32) -> Self {
        SteerGoal {
            point,
            arrive_m,
            anchor_vel: [0.0; 3],
        }
    }

    /// A goal attached to a moving anchor (a formation slot on a moving leader).
    pub fn moving(point: [f32; 3], arrive_m: f32, anchor_vel: [f32; 3]) -> Self {
        SteerGoal {
            point,
            arrive_m,
            anchor_vel,
        }
    }
}

/// Whether (and at whom) a wingman may fire this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FireGate {
    /// Fire at whatever the aim layer picks, subject to the friendly cone.
    WeaponsFree,
    /// Hold fire entirely.
    CeaseFire,
    /// Fire only at this target, still subject to the friendly cone.
    TargetOnly(u32),
}

/// A wingman's resolved behavior for one tick. `option_bias` overrides or augments the utility decision; `steer`
/// and `fire` are consumed by the motor and the fire seam ([`drive`]); `callout` is voiced by the host where it has a
/// line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WingmanPlan {
    /// Where to steer, or `None` to leave movement to the scorer.
    pub steer: Option<SteerGoal>,
    /// The fire gate.
    pub fire: FireGate,
    /// What to say, if anything.
    pub callout: Option<CalloutId>,
    /// The option this plan biases, if any.
    pub option_bias: Option<OptionId>,
}

// ---------------------------------------------------------------------------
// Callouts
// ---------------------------------------------------------------------------

/// Wingman callout intents. The host maps each to its own voice or announcer line ([`CalloutId::name`] is a stable
/// key); a callout with no line plays nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalloutId {
    /// Order acknowledged.
    Ack,
    /// Contact; engaging it.
    Engaging,
    /// The hull is almost gone.
    HullCritical,
    /// Needs repair.
    NeedRepair,
    /// Low ammo.
    LowAmmo,
    /// An enemy is behind the leader.
    EnemyBehind,
    /// Incoming fire.
    Incoming,
}

impl CalloutId {
    /// Every callout, in declaration order.
    pub const ALL: [CalloutId; 7] = [
        CalloutId::Ack,
        CalloutId::Engaging,
        CalloutId::HullCritical,
        CalloutId::NeedRepair,
        CalloutId::LowAmmo,
        CalloutId::EnemyBehind,
        CalloutId::Incoming,
    ];

    /// The callout's stable key (`ack`, `engaging`, `hull_critical`, `need_repair`, `low_ammo`, `enemy_behind`,
    /// `incoming`), for the host's line table.
    pub fn name(self) -> &'static str {
        match self {
            CalloutId::Ack => "ack",
            CalloutId::Engaging => "engaging",
            CalloutId::HullCritical => "hull_critical",
            CalloutId::NeedRepair => "need_repair",
            CalloutId::LowAmmo => "low_ammo",
            CalloutId::EnemyBehind => "enemy_behind",
            CalloutId::Incoming => "incoming",
        }
    }
}

// ---------------------------------------------------------------------------
// Reporting, repair and ammo runs
// ---------------------------------------------------------------------------

/// Below this hull fraction the report is "hull critical" (below [`LOW_HULL_PCT`] it is "need repair").
pub const CRITICAL_HULL_PCT: f32 = 0.15;

/// Low-hull / low-ammo report for a unit, or `None` when it is fine. Hull
/// reports are order-stable (critical before repair before ammo).
pub fn report(unit: &AiUnitView) -> Option<CalloutId> {
    if unit.hull_pct <= CRITICAL_HULL_PCT {
        Some(CalloutId::HullCritical)
    } else if unit.hull_pct <= LOW_HULL_PCT {
        Some(CalloutId::NeedRepair)
    } else if ammo_pct(unit) <= LOW_AMMO_PCT {
        Some(CalloutId::LowAmmo)
    } else {
        None
    }
}

/// Nearest repair point for a low-hull unit (the repair run). `points` are the team's repair structures. `None` =
/// healthy, or nothing to run to.
pub fn repair_goal(unit: &AiUnitView, points: &[[f32; 3]]) -> Option<SteerGoal> {
    if unit.hull_pct > LOW_HULL_PCT {
        return None;
    }
    nearest_xz(unit.pos, points).map(|point| SteerGoal::still(point, WINGMAN_ARRIVE_M))
}

/// Nearest ammo point for a dry unit (the ammo run).
pub fn ammo_goal(unit: &AiUnitView, points: &[[f32; 3]]) -> Option<SteerGoal> {
    if ammo_pct(unit) > LOW_AMMO_PCT {
        return None;
    }
    nearest_xz(unit.pos, points).map(|point| SteerGoal::still(point, WINGMAN_ARRIVE_M))
}

/// The hard friendly-fire gate: a wingman holds fire when any friendly body sits inside the shot cone toward
/// `target`. This is the "no friendly fire into the player's line" acceptance.
pub fn fire_blocked(origin: [f32; 3], target: [f32; 3], friendlies: &[[f32; 3]]) -> bool {
    line_of_fire(origin, target, friendlies)
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// How far a brawler parks off an attack target.
const ATTACK_ARRIVE_M: f32 = 8.0;

fn dist_xz(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dz = a[2] - b[2];
    crate::math::sqrt(dx * dx + dz * dz)
}

/// The position order after distance: X, then Z, then Y, each by [`det_cmp`] (a total order).
fn cmp_pos(a: [f32; 3], b: [f32; 3]) -> std::cmp::Ordering {
    det_cmp(a[0], b[0])
        .then_with(|| det_cmp(a[2], b[2]))
        .then_with(|| det_cmp(a[1], b[1]))
}

/// The nearest point (XZ), equal distances broken on the position ([`cmp_pos`]): a total order, so the pick never
/// depends on the slice order (ADR-8655). Only equal points tie, and they are the same answer.
fn nearest_xz(from: [f32; 3], points: &[[f32; 3]]) -> Option<[f32; 3]> {
    points
        .iter()
        .copied()
        .min_by(|a, b| det_cmp(dist_xz(from, *a), dist_xz(from, *b)).then_with(|| cmp_pos(*a, *b)))
}

fn unit_pose(world: &AiWorld, id: u32) -> Option<([f32; 3], f32, [f32; 3])> {
    world
        .units
        .iter()
        .find(|u| u.id == id)
        .map(|u| (u.pos, u.yaw, u.vel))
}

/// Escort and regroup slot on the leader: the spaced V (`escort_goal`), not the team centroid and not one shared
/// wedge point. Falls back to a plain wedge only when the leader is absent from the world.
fn escort_point(
    unit: &AiUnitView,
    world: &AiWorld,
    leader_id: u32,
    slot_index: usize,
) -> ([f32; 3], [f32; 3]) {
    if let Some(leader) = world.units.iter().find(|u| u.id == leader_id) {
        return (escort_goal(unit, leader, slot_index), leader.vel);
    }
    let (anchor, yaw, vel) = unit_pose(world, leader_id).unwrap_or((unit.pos, unit.yaw, [0.0; 3]));
    (
        wingman_slot(WingmanFormation::Wedge, slot_index, anchor, yaw, false),
        vel,
    )
}

/// The leader of [`WingmanCommand::EscortCommander`] for `unit`: the lowest-id allied commander hull, else the
/// lowest-id allied hull (allied hull = same team, another entity, a unit, not on foot: the combat layer's escort
/// leader rule, read here by id so it does not lean on the world's slice order). `None` when `unit` is itself the
/// commander or has no allied hull.
fn commander_of(unit: &AiUnitView, world: &AiWorld) -> Option<u32> {
    if unit.is_commander {
        return None;
    }
    let allied = || {
        world
            .units
            .iter()
            .filter(|u| u.team == unit.team && u.id != unit.id && u.is_unit && !u.on_foot)
    };
    allied()
        .filter(|u| u.is_commander)
        .map(|u| u.id)
        .min()
        .or_else(|| allied().map(|u| u.id).min())
}

fn unit_pos(world: &AiWorld, id: u32) -> Option<[f32; 3]> {
    world.units.iter().find(|u| u.id == id).map(|u| u.pos)
}

/// The nearest enemy's position (XZ), equal distances broken on the lower entity id, then the position: a total
/// order (ADR-8655). On the documented id-ordered world that is the first nearest unit, as before.
fn nearest_enemy(world: &AiWorld, team: u8, pos: [f32; 3]) -> Option<[f32; 3]> {
    world
        .units
        .iter()
        .filter(|u| u.team != team && u.team != 0)
        .min_by(|a, b| {
            det_cmp(dist_xz(pos, a.pos), dist_xz(pos, b.pos))
                .then_with(|| a.id.cmp(&b.id))
                .then_with(|| cmp_pos(a.pos, b.pos))
        })
        .map(|u| u.pos)
}

/// Resolve a unit's wingman command into its per-tick behavior.
///
/// `slot_index` is the unit's formation slot (from its squad, or from the
/// group router). Pure and deterministic: reads only `world`, orders nothing.
pub fn plan(unit: &AiUnitView, command: &WingmanCommand, world: &AiWorld, slot_index: usize) -> WingmanPlan {
    match *command {
        WingmanCommand::Follow {
            leader,
            formation,
            close,
        } => {
            let (anchor, yaw, vel) = unit_pose(world, leader).unwrap_or((unit.pos, unit.yaw, [0.0; 3]));
            let point = wingman_slot(formation, slot_index, anchor, yaw, close);
            WingmanPlan {
                steer: Some(SteerGoal::moving(point, WINGMAN_ARRIVE_M, vel)),
                fire: FireGate::WeaponsFree,
                callout: Some(CalloutId::Ack),
                option_bias: Some(OptionId::Regroup),
            }
        }
        WingmanCommand::Advance { leader } => {
            let (anchor, yaw, vel) = unit_pose(world, leader).unwrap_or((unit.pos, unit.yaw, [0.0; 3]));
            // Abreast line ahead of the anchor: the Line shape shifted forward
            // past the anchor by the order book's lead distance, so the screen
            // pushes while the followers behind keep out of the fire lane.
            let slot = wingman_slot(WingmanFormation::Line, slot_index, anchor, yaw, false);
            let (s, c) = (crate::math::sin(yaw), crate::math::cos(yaw));
            let shift = ADVANCE_LEAD_M + WINGMAN_SPACING_M;
            let point = [slot[0] + s * shift, slot[1], slot[2] + c * shift];
            WingmanPlan {
                steer: Some(SteerGoal::moving(point, WINGMAN_ARRIVE_M, vel)),
                fire: FireGate::WeaponsFree,
                callout: Some(CalloutId::Ack),
                option_bias: Some(OptionId::Push),
            }
        }
        WingmanCommand::Flank { leader } => {
            let (anchor, yaw, vel) = unit_pose(world, leader).unwrap_or((unit.pos, unit.yaw, [0.0; 3]));
            // Abreast of the anchor, off to the side at the lane edge and
            // stepping out by file: even slots take the right side, odd slots
            // the left. Forward offset is zero, so the slot always clears the
            // leader's fire lane.
            let side = if slot_index.is_multiple_of(2) { 1.0 } else { -1.0 };
            let file = (slot_index / 2) as f32;
            let right = side * (WINGMAN_LANE_CLEAR_M + file * WINGMAN_COLUMN_GAP_M);
            let (s, c) = (crate::math::sin(yaw), crate::math::cos(yaw));
            let point = [anchor[0] + c * right, anchor[1], anchor[2] - s * right];
            WingmanPlan {
                steer: Some(SteerGoal::moving(point, WINGMAN_ARRIVE_M, vel)),
                fire: FireGate::WeaponsFree,
                callout: Some(CalloutId::Ack),
                option_bias: Some(OptionId::Flank),
            }
        }
        WingmanCommand::AttackTarget { target } => {
            let point = unit_pos(world, target).unwrap_or(unit.pos);
            WingmanPlan {
                steer: Some(SteerGoal::still(point, ATTACK_ARRIVE_M)),
                fire: FireGate::TargetOnly(target),
                callout: Some(CalloutId::Engaging),
                option_bias: Some(OptionId::FocusFire),
            }
        }
        WingmanCommand::AttackAtWill => {
            let steer = nearest_enemy(world, unit.team, unit.pos)
                .map(|point| SteerGoal::still(point, ATTACK_ARRIVE_M));
            WingmanPlan {
                steer,
                fire: FireGate::WeaponsFree,
                callout: Some(CalloutId::Ack),
                option_bias: None,
            }
        }
        WingmanCommand::Escort { leader } => {
            let (point, vel) = escort_point(unit, world, leader, slot_index);
            WingmanPlan {
                steer: Some(SteerGoal::moving(point, WINGMAN_ARRIVE_M, vel)),
                fire: FireGate::WeaponsFree,
                callout: Some(CalloutId::Ack),
                option_bias: Some(OptionId::Escort),
            }
        }
        WingmanCommand::EscortCommander => match commander_of(unit, world) {
            Some(leader) => plan(unit, &WingmanCommand::Escort { leader }, world, slot_index),
            None => plan(
                unit,
                &WingmanCommand::Hold {
                    point: [0.0, 0.0, 0.0],
                },
                world,
                slot_index,
            ),
        },
        WingmanCommand::Hold { point } => {
            // An unset point (an order book's Hold has no coordinates) holds
            // where the unit stands; it never means the map origin.
            let at = if point == [0.0, 0.0, 0.0] { unit.pos } else { point };
            WingmanPlan {
                steer: Some(SteerGoal::still(at, WINGMAN_ARRIVE_M)),
                fire: FireGate::WeaponsFree,
                callout: Some(CalloutId::Ack),
                option_bias: Some(OptionId::Hold),
            }
        }
        WingmanCommand::DefendPoint { point } => WingmanPlan {
            steer: Some(SteerGoal::still(point, WINGMAN_ARRIVE_M)),
            fire: FireGate::WeaponsFree,
            callout: Some(CalloutId::Ack),
            option_bias: Some(OptionId::Defend),
        },
        WingmanCommand::DefendEntity { entity } => {
            let point = unit_pos(world, entity).unwrap_or(unit.pos);
            WingmanPlan {
                steer: Some(SteerGoal::still(point, WINGMAN_ARRIVE_M)),
                fire: FireGate::WeaponsFree,
                callout: Some(CalloutId::Ack),
                option_bias: Some(OptionId::Defend),
            }
        }
        WingmanCommand::Patrol { a, b, toward_b } => {
            let point = if toward_b { b } else { a };
            WingmanPlan {
                steer: Some(SteerGoal::still(point, WINGMAN_ARRIVE_M)),
                fire: FireGate::WeaponsFree,
                callout: Some(CalloutId::Ack),
                option_bias: None,
            }
        }
        WingmanCommand::CollectScrap => WingmanPlan {
            // The host's economy owns resource routing; the wingman layer only
            // biases the scorer and keeps the unit weapons-free to defend itself.
            steer: None,
            fire: FireGate::WeaponsFree,
            callout: Some(CalloutId::Ack),
            option_bias: Some(OptionId::CollectScrap),
        },
        WingmanCommand::Regroup { leader } => {
            let (point, vel) = escort_point(unit, world, leader, slot_index);
            WingmanPlan {
                steer: Some(SteerGoal::moving(point, WINGMAN_ARRIVE_M, vel)),
                fire: FireGate::WeaponsFree,
                callout: Some(CalloutId::Ack),
                option_bias: Some(OptionId::Regroup),
            }
        }
        WingmanCommand::Retreat => {
            // Retreat to Re-up: when the team holds a repair or supply point,
            // withdraw to it; else step away from the nearest threat. Either way
            // the guns stay free — a fighting withdrawal, firing while falling back.
            let point = match reup_point(unit, world) {
                Some(rp) => rp,
                None => {
                    let enemy = nearest_enemy(world, unit.team, unit.pos).unwrap_or(unit.pos);
                    let dx = unit.pos[0] - enemy[0];
                    let dz = unit.pos[2] - enemy[2];
                    let len = det_max(crate::math::sqrt(dx * dx + dz * dz), 0.01);
                    [
                        unit.pos[0] + dx / len * WINGMAN_RETREAT_STEP_M,
                        unit.pos[1],
                        unit.pos[2] + dz / len * WINGMAN_RETREAT_STEP_M,
                    ]
                }
            };
            WingmanPlan {
                steer: Some(SteerGoal::still(point, WINGMAN_ARRIVE_M)),
                // Fighting withdrawal: the guns stay free, the body leaves.
                fire: FireGate::WeaponsFree,
                callout: Some(CalloutId::Ack),
                option_bias: Some(OptionId::Retreat),
            }
        }
        WingmanCommand::PickupPilot { pilot } => {
            let point = unit_pos(world, pilot).unwrap_or(unit.pos);
            WingmanPlan {
                steer: Some(SteerGoal::still(point, 1.0)),
                fire: FireGate::CeaseFire,
                callout: Some(CalloutId::Ack),
                option_bias: None,
            }
        }
        WingmanCommand::CeaseFire => WingmanPlan {
            steer: None,
            fire: FireGate::CeaseFire,
            callout: Some(CalloutId::Ack),
            option_bias: None,
        },
        WingmanCommand::WeaponsFree => WingmanPlan {
            steer: None,
            fire: FireGate::WeaponsFree,
            callout: Some(CalloutId::Ack),
            option_bias: None,
        },
        WingmanCommand::Stance(stance) => {
            let (fire, bias) = match stance {
                Stance::Aggressive => (FireGate::WeaponsFree, Some(OptionId::Push)),
                Stance::Defensive => (FireGate::WeaponsFree, Some(OptionId::Hold)),
                Stance::HoldFire => (FireGate::CeaseFire, None),
            };
            WingmanPlan {
                steer: None,
                fire,
                callout: Some(CalloutId::Ack),
                option_bias: bias,
            }
        }
    }
}

/// Route a command to a group (or one unit) and resolve every member's plan,
/// assigning formation slots in deterministic (sorted-id) order. This is the
/// group-vs-individual seam: `unit_ids` holds one id for an individual order
/// or the whole group for a group order; the caller already decided which.
/// Ids missing from `world` get no plan.
pub fn route(command: &WingmanCommand, unit_ids: &[u32], world: &AiWorld) -> BTreeMap<u32, WingmanPlan> {
    let mut ids: Vec<u32> = unit_ids.to_vec();
    ids.sort_unstable();
    ids.into_iter()
        .enumerate()
        .filter_map(|(index, id)| {
            let unit = world.units.iter().find(|u| u.id == id)?;
            Some((id, plan(unit, command, world, index)))
        })
        .collect()
}

/// Drive a resolved plan through the motor and combat layer. `plan.steer` is handed to
/// [`super::arrive_moving`] (the hover inertia is the sim's, applied by its hover step); `plan.fire` is handed to
/// [`super::fire_intent`] (the hard fire gate plus target priority). The allied keep-out
/// ([`super::separate_from_allies`]) comes first, then the Re-up override ([`super::reup_needed`]).
pub fn drive<L: LineOfSight + Copy>(
    unit: &AiUnitView,
    plan: &WingmanPlan,
    world: &AiWorld,
    board: &SightingsBoard,
    coord: &CoordState,
    los: L,
    rng: &mut AiRng,
) -> AiUnitOutput {
    let targets = TeamTargets::for_unit(unit, world, board);
    drive_from(unit, plan, world, &targets, coord, los, rng)
}

/// [`drive`] on targets resolved once for the whole run ([`TeamTargets`]).
pub fn drive_from<L: LineOfSight + Copy>(
    unit: &AiUnitView,
    plan: &WingmanPlan,
    world: &AiWorld,
    targets: &TeamTargets,
    coord: &CoordState,
    los: L,
    rng: &mut AiRng,
) -> AiUnitOutput {
    let steer = if let Some(separate) = separate_from_allies(unit, world) {
        // Separation before arrive: a hull inside the allied keep-out (radius
        // sum + 2 m, which covers the human commander's hull centre) breaks away
        // before it works any goal, so no wingman ever parks on the player and
        // fills the cockpit view.
        Some(separate)
    } else if reup_needed(unit.hull_pct, ammo_pct(unit)) {
        // Re-up override: a unit at or below the hull/ammo thresholds walks
        // its body to the nearest Re-up point under any command. The fire
        // gate below is untouched, so it falls back while firing.
        match reup_point(unit, world) {
            Some(rp) => Some(arrive_moving(unit, rp, WINGMAN_ARRIVE_M, [0.0; 3])),
            None => plan
                .steer
                .map(|g| arrive_moving(unit, g.point, g.arrive_m, g.anchor_vel)),
        }
    } else {
        plan.steer
            .map(|g| arrive_moving(unit, g.point, g.arrive_m, g.anchor_vel))
    };
    let fire = match plan.fire {
        FireGate::CeaseFire => None,
        FireGate::WeaponsFree => fire_intent_from(unit, world, targets, coord, los, rng, None),
        FireGate::TargetOnly(target) => fire_intent_from(unit, world, targets, coord, los, rng, Some(target)),
    };
    AiUnitOutput {
        steer,
        fire,
        callout: None,
    }
}

// ---------------------------------------------------------------------------
// The wingman override of a combat run
// ---------------------------------------------------------------------------

/// One commanded unit: its entity id, its command and its formation slot (from [`assign_formation_slots`] or the
/// host's squad).
pub type CommandedUnit = (u32, WingmanCommand, usize);

/// Commit a command's option at once, so the action is readable even before the unit's next decision phase: the
/// option becomes current and the hysteresis counters restart.
pub fn commit_bias(decision: &mut Decision, option: OptionId) {
    decision.current = Some(option);
    decision.held_ticks = 0;
    decision.challenger = None;
    decision.challenger_ticks = 0;
}

/// The wingman override of a combat run: every unit named in `orders` resolves its command ([`plan`]) and drives it
/// through the motor and the fire seam ([`drive_from`]), replacing its entry in `outputs` (one per world unit, in
/// world order, as [`super::combat_run_from`] returns them). Returns the options to commit ([`commit_bias`]), in
/// world order. Each commanded unit draws from the same per-unit-per-tick stream as the combat run.
pub fn apply_commands<L: LineOfSight + Copy>(
    world: &AiWorld,
    targets: &TeamTargets,
    coord: &CoordState,
    los: L,
    orders: &[CommandedUnit],
    outputs: &mut [AiUnitOutput],
) -> Vec<(u32, OptionId)> {
    let by_id: BTreeMap<u32, (&WingmanCommand, usize)> =
        orders.iter().map(|(id, cmd, slot)| (*id, (cmd, *slot))).collect();
    let mut biases = Vec::new();
    for (index, unit) in world.units.iter().enumerate() {
        let Some((cmd, slot)) = by_id.get(&unit.id) else {
            continue;
        };
        let plan = plan(unit, cmd, world, *slot);
        if let Some(bias) = plan.option_bias {
            biases.push((unit.id, bias));
        }
        let mut rng = AiRng::new(AiRng::mix(
            world.world_seed ^ world.tick.wrapping_mul(0x9E37_79B9_7F4A_7C15),
            unit.id as u64,
        ));
        if let Some(out) = outputs.get_mut(index) {
            *out = drive_from(unit, &plan, world, targets, coord, los, &mut rng);
        }
    }
    biases
}

/// The combat run with the wingman override: [`super::combat_run_from`] for every unit on its committed decision,
/// then [`apply_commands`] for the units named in `orders`, whose command's option is committed in `decisions`
/// ([`commit_bias`]). One output per world unit, in world order.
pub fn run_commanded<L: LineOfSight + Copy>(
    world: &AiWorld,
    board: &SightingsBoard,
    coord: &CoordState,
    los: L,
    decisions: &mut BTreeMap<u32, Decision>,
    orders: &[CommandedUnit],
) -> Vec<AiUnitOutput> {
    let _span = tracing::trace_span!(
        "regia_detsim::ai::wingman",
        tick = world.tick,
        orders = orders.len()
    )
    .entered();
    let targets = TeamTargets::build(world, board);
    let mut outputs = combat_run_from(world, &targets, coord, los, decisions);
    for (id, option) in apply_commands(world, &targets, coord, los, orders, &mut outputs) {
        commit_bias(decisions.entry(id).or_default(), option);
    }
    outputs
}

#[cfg(test)]
mod tests;
