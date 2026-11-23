// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/ai/mod.rs, commit c9513623f, tag regia-absorb-4), ADR-8012.
// Mark: adapt (lines 998-1058 and 1075-1103, plan row S-12, ADR-8516; lines 1196-1201 for `HoverView::from_params`).
// `HoverView`, the unit view, `AiWorld`, `SteerCommand`, `AiUnitOutput` and `CoordState` are DO27's read view and
// output types; named changes:
// - the hover view's fields take the neutral names of ADR-8012 A-8 (`omega_turn` -> `yaw_rate_at_speed`,
//   `accel_brake` -> `brake_accel`); `HoverView::from_params` is DO27's pipeline mapping from the hover parameters
//   (its two float `max` calls through `det_max`, ADR-8085);
// - the unit view keeps the fields the motor and combat layers read. DO27's hull and weapon stem tables (game
//   content) become per-unit data the host fills from its own tables: `max_hull`, `target_value`, `weapon_dps` and
//   the `reup` role (DO27's repair and supply stems); the entity class becomes `is_unit` and the on-foot crew flag
//   `on_foot`; the gate's mortar range is the S-11 setting `mortar_min_range_m`. The squad, order, crew-class,
//   known-enemy, sensor and lit fields belong to the wingman, mode-bot and perception layers (plan rows S-10, S-13)
//   and stay with the host;
// - `CoordState` holds one focus per team in a `Vec` (DO27's five-team array becomes the `team_slots` setting,
//   five by default); serde derives and doc comments are added.

//! The combat layer's read view and outputs: [`AiUnitView`] (one unit, frozen for the tick), [`AiWorld`] (the tick's
//! snapshot in entity-id order), [`SteerCommand`] and [`AiUnitOutput`] (what the layer writes back), [`HoverView`]
//! (the handling the motor reads) and [`CoordState`] (the team focus-fire cache).

use serde::{Deserialize, Serialize};

use super::aim::{AmmoView, FireIntent, MORTAR_MIN_RANGE_M, ShooterView, Stance};
use super::difficulty::AiLevel;
use crate::float::det_max;
use crate::hover::HoverParams;

/// Hull points of a unit whose table entry is unknown (a contact that left the world view): DO27's default row.
pub const DEFAULT_MAX_HULL: f32 = 500.0;

/// Target value of a unit whose table entry is unknown: DO27's default row (unknown targets rank last).
pub const DEFAULT_TARGET_VALUE: f32 = 100.0;

/// Collision radius of [`AiUnitView::new`], metres.
pub const DEFAULT_RADIUS_M: f32 = 4.0;

/// Teams [`CoordState::default`] holds a focus for (DO27's five).
pub const DEFAULT_TEAM_SLOTS: usize = 5;

/// The handling the motor reads: how fast the hull goes, how fast it turns at speed and how hard it brakes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HoverView {
    /// Forward speed cap, m/s (at least 1).
    pub top_speed: f32,
    /// Yaw rate at full speed, rad/s (at least 0.5).
    pub yaw_rate_at_speed: f32,
    /// Braking deceleration, m/s².
    pub brake_accel: f32,
    /// Speed share bled per unit of turn.
    pub turn_bleed: f32,
}

impl HoverView {
    /// The view of a hull's hover parameters: the forward speed cap (floored at 1 m/s), the yaw rate at speed
    /// (floored at 0.5 rad/s), the braking deceleration and the turn bleed.
    pub fn from_params(params: &HoverParams) -> Self {
        HoverView {
            top_speed: det_max(params.max_forward_speed, 1.0),
            yaw_rate_at_speed: det_max(params.yaw_rate_at_speed, 0.5),
            brake_accel: params.brake_accel,
            turn_bleed: params.turn_bleed,
        }
    }
}

impl Default for HoverView {
    /// The view of the engine's default hull ([`HoverParams::default`]).
    fn default() -> Self {
        HoverView::from_params(&HoverParams::default())
    }
}

/// Which Re-up a structure (or a support unit) gives its team's damaged or dry units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ReupRole {
    /// Not a Re-up point.
    #[default]
    None,
    /// Repairs hulls.
    Repair,
    /// Re-arms hulls.
    Supply,
}

/// One unit (or structure) of the tick's snapshot, frozen for the tick. The host builds the views in entity-id
/// order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiUnitView {
    /// Entity id.
    pub id: u32,
    /// Team index.
    pub team: u8,
    /// A mobile unit (a hull); `false` is a structure. Seekers lock units only, mortars shell structures only, and
    /// only units keep the allied keep-out.
    pub is_unit: bool,
    /// Crew on foot: not a hull, so allied hulls do not keep out of it (they must reach it to pick it up).
    pub on_foot: bool,
    /// World position.
    pub pos: [f32; 3],
    /// Heading, radians; forward is `(sin yaw, cos yaw)` in XZ.
    pub yaw: f32,
    /// Velocity, m/s.
    pub vel: [f32; 3],
    /// Hull fraction left, 0..1.
    pub hull_pct: f32,
    /// Full hull points (the host's hull table).
    pub max_hull: f32,
    /// How much a shooter values this target before distance (the host's target table: structures that decide
    /// the fight first, then combat hulls, then utility and scouts).
    pub target_value: f32,
    /// Damage per second of all the unit's mounted weapons, summed in slot order (the host's weapon table).
    pub weapon_dps: f32,
    /// The Re-up this entity gives its team.
    pub reup: ReupRole,
    /// The unit's hardpoints.
    pub ammo: AmmoView,
    /// The unit's handling.
    pub hover: HoverView,
    /// Fire stance.
    pub stance: Stance,
    /// The unit may fire (its order allows it).
    pub weapons_free: bool,
    /// The unit's difficulty level (the profile key).
    pub level: AiLevel,
    /// Collision radius, metres: escort slots and the allied keep-out are sized from it.
    pub radius: f32,
    /// The team's commander hull (a human player's own hull): escorts form on it and allied hulls keep out of its
    /// disc.
    pub is_commander: bool,
    /// Beyond this range a mortar is chosen against a structure ([`MORTAR_MIN_RANGE_M`] by default).
    pub mortar_min_range_m: f32,
}

impl AiUnitView {
    /// A full-hull, weapons-free, aggressive Medium unit of `team` at `pos` facing +Z at rest, with no weapons,
    /// the default table entries ([`DEFAULT_MAX_HULL`], [`DEFAULT_TARGET_VALUE`]), the default hull's handling and a
    /// [`DEFAULT_RADIUS_M`] radius.
    pub fn new(id: u32, team: u8, pos: [f32; 3]) -> Self {
        AiUnitView {
            id,
            team,
            is_unit: true,
            on_foot: false,
            pos,
            yaw: 0.0,
            vel: [0.0; 3],
            hull_pct: 1.0,
            max_hull: DEFAULT_MAX_HULL,
            target_value: DEFAULT_TARGET_VALUE,
            weapon_dps: 0.0,
            reup: ReupRole::None,
            ammo: AmmoView::default(),
            hover: HoverView::default(),
            stance: Stance::Aggressive,
            weapons_free: true,
            level: AiLevel::Medium,
            radius: DEFAULT_RADIUS_M,
            is_commander: false,
            mortar_min_range_m: MORTAR_MIN_RANGE_M,
        }
    }

    /// What the fire gate reads of this unit (S-11's [`ShooterView`]).
    pub fn shooter(&self) -> ShooterView {
        ShooterView {
            pos: self.pos,
            weapons_free: self.weapons_free,
            stance: self.stance,
            ammo: self.ammo.clone(),
            mortar_min_range_m: self.mortar_min_range_m,
        }
    }
}

/// The read-only snapshot built once per tick.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiWorld {
    /// The sim tick.
    pub tick: u64,
    /// The match seed (every per-unit RNG stream mixes it in).
    pub world_seed: u64,
    /// Every unit and structure, in entity-id order.
    pub units: Vec<AiUnitView>,
}

/// One tick's movement axes for the hover step: the same three inputs a player drives. A command is only ever a
/// rate, so aim never fights steering.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SteerCommand {
    /// Forward (+) or reverse (-) thrust, -1..1.
    pub throttle: f32,
    /// Right (+) or left (-) strafe, -1..1.
    pub strafe: f32,
    /// Turn demand, -1..1; positive turns right (yaw increases).
    pub steer: f32,
}

/// Per-unit output, written once per tick and applied after every unit's decision.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct AiUnitOutput {
    /// Movement, or `None` to leave the hull's inputs alone.
    pub steer: Option<SteerCommand>,
    /// A shot, or `None` to hold fire.
    pub fire: Option<FireIntent>,
    /// A host-defined callout id to voice, if any.
    pub callout: Option<u32>,
}

/// The team focus-fire cache: one focus target per team. Not a planner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordState {
    /// The focus target of each team, indexed by team.
    pub focus_by_team: Vec<Option<u32>>,
}

impl Default for CoordState {
    /// No focus, for [`DEFAULT_TEAM_SLOTS`] teams.
    fn default() -> Self {
        CoordState::with_teams(DEFAULT_TEAM_SLOTS)
    }
}

impl CoordState {
    /// No focus, for `team_slots` teams (teams `0..team_slots`).
    pub fn with_teams(team_slots: usize) -> Self {
        CoordState {
            focus_by_team: vec![None; team_slots],
        }
    }

    /// The focus target of `team`, if any (`None` for a team past the slots).
    pub fn focus_id(&self, team: u8) -> Option<u32> {
        self.focus_by_team.get(team as usize).copied().flatten()
    }

    /// Set (or clear) the focus target of `team`; a team past the slots is ignored.
    pub fn set_focus(&mut self, team: u8, id: Option<u32>) {
        if let Some(slot) = self.focus_by_team.get_mut(team as usize) {
            *slot = id;
        }
    }
}
