// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/ai/mod.rs, commit d6ed37677), ADR-8012.
// Mark: adapt (lines 871-956, plan row S-09). `AiRng` and `AiSchedule` are DO27's code byte for byte apart from
// rustfmt and doc comments. Named changes: the schedule's cadences also exist as a host setting (`AiCadence`,
// DO27's constants are its default); `AiRng::tick_stream` names the per-unit-per-tick seed formula DO27's
// later AI layers spell out inline (same bits). The rest of DO27's `mod.rs` is game code (read view, coordinator,
// gatherer and cover helpers) or belongs to plan rows S-10 to S-13; the decision step of its pipeline
// (lines 1389-1413) is adapted in `scheduler.rs`.

//! The deterministic AI core for lockstep games (plan row S-09, ADR-8152).
//!
//! Everything here is a pure function of the sim state the host passes in, so every peer of a lockstep match
//! commits the same decisions on the same tick:
//!
//! - [`AiRng`]: a seeded xorshift64 reseeded per unit (and per tick) from the world seed and the entity id,
//!   never from the wall clock or thread order.
//! - [`AiSchedule`] and [`AiCadence`]: the time-sliced schedule. A unit re-scores its options on the ticks
//!   where `tick % cadence == entity_id % cadence`, so a crowd's scoring is spread evenly over the cadence
//!   window and each tick's slice is known in advance.
//! - The utility scorer ([`score`], [`ResponseCurve`], [`Consideration`], [`AiOption`]): an Infinite-Axis
//!   Utility System with the product rule (a multiplier consideration at zero zeroes the option). Curves
//!   use [`crate::math`] only.
//! - [`AiCore`]: runs the due slice each tick in entity-id order, hands each unit's scores to a
//!   [`DecisionPolicy`] ([`ArgMax`] here; the hysteresis loop of plan row S-11 plugs in the same way) and
//!   keeps the committed decisions in a `BTreeMap` by entity id.
//!
//! Perception, the influence grid, tactics, aim, motor and squads build on these types (plan rows S-10 to
//! S-13). The book page is "Deterministic AI core" (`book/agents/det-ai-core.md`).
//!
//! Plan row S-10 (ADR-8395) adds:
//!
//! - [`LineOfSight`]: the line-of-sight seam. [`OpenField`], [`HeightfieldLos`], [`LosFn`] and the
//!   [`MouthWall`] test double implement it here; `regia_world_geom` implements it for its terrain sampler
//!   and tunnel mesh.
//! - [`SightingsBoard`]: the team-shared sighting board (direct line of sight within sight range, shared
//!   radar, decay and forgetting per team's [`PerceptionParams`]).
//! - [`InfluenceGrid`]: the 8 m threat and control grid, stamped and decayed one step per tick, with
//!   [`flank_bearing`] and [`line_of_fire`]. The book page is "Deterministic perception and influence"
//!   (`book/agents/det-ai-perception.md`).
//!
//! Plan row S-11 (ADR-8462) adds:
//!
//! - [`Hysteresis`] and [`decide`]: the anti-flip-flop decision loop (a challenger must lead by a margin for a
//!   number of decisions in a row), as the core's [`DecisionPolicy`]; [`catalogue`]: the twelve options with
//!   curves keyed to a profile's [`Thresholds`], and [`bias`].
//! - [`DifficultyProfile`] and [`AiLevel`]: difficulty as decision quality only (reaction, awareness, aim error,
//!   repertoire, coordination, caution); no stat field.
//! - [`lead_point`], [`aim_error`], [`choose_slot`], [`fire_gate`], [`ballistic_aim`], [`seeker_can_lock`] and
//!   [`nearest_in_band`]: lead targeting, human-like aim error and the deterministic fire gate. The book page is
//!   "Deterministic tactics, difficulty and aim" (`book/agents/det-ai-tactics.md`).
//!
//! Plan row S-12 (ADR-8516) adds:
//!
//! - The combat layer's read view and outputs: [`AiUnitView`], [`AiWorld`], [`HoverView`], [`SteerCommand`],
//!   [`AiUnitOutput`] and the team focus cache [`CoordState`].
//! - The motor: [`arrive`], [`arrive_moving`], [`pursue`] ([`Pursuit`]), [`orbit`], [`kite`], [`evade`],
//!   [`formation_slot`] and the hover energy manager [`energy_throttle`]; steer commands only, never a yaw write.
//! - The combat composition: [`target_priority`], [`fire_intent`], [`unit_output`] and [`combat_run`] (option to
//!   motor and fire gate, mortar arcs at structures beyond the mortar range), the Re-up rule ([`reup_needed`],
//!   [`reup_point`]) and the escort V with the allied keep-out ([`separate_from_allies`]). The book page is
//!   "Deterministic motor and combat" (`book/agents/det-ai-combat.md`).
//!
//! [`TeamRosters`] and [`SlotIndex`] (re-copied from regia-absorb-4's SIM_TICK_PERF fix): each team's rows
//! and an entity id -> slot index, built once per tick, so the layers above read one team's rows instead of
//! filtering the whole world once per unit, with the same results bit for bit.
//!
//! [`targets`] (audit gap AI-2): a versioned target selector. [`TargetRule::Exhaustive`] (rules version 1) is
//! the every-enemy-every-tick loop bit for bit; [`TargetRule::Ringed`] (version 2) scores at most a few
//! in-range candidates on a staggered cadence. The game keeps the score; the engine owns candidates, cadence
//! and ties.
//!
//! Gap AI-4 (ADR-8518) adds [`PerceptionIndex`] (the sighting refresh on the spatial index,
//! [`SightingsBoard::refresh_with`]; [`SightingsBoard::refresh_linear`] keeps the all-pairs scan as the
//! oracle) and [`AiTickWorld`] / [`AiWorldView`] (per-unit views that borrow the per-team facts).
//!
//! Plan row S-13 (ADR-8554) adds:
//!
//! - The wingman layer: the four formation shapes ([`WingmanFormation`], [`wingman_slot`],
//!   [`assign_formation_slots`], [`level_formation_anchor`]), the command vocabulary ([`WingmanCommand`]) with the
//!   orders-to-plans interface ([`WingmanOrder`], [`TeamDirective`]), the per-unit plan ([`plan`], [`route`],
//!   [`WingmanPlan`]), the reports and Re-up runs ([`report`], [`repair_goal`], [`ammo_goal`]) and the plan-to-motor
//!   drive ([`drive`]).
//! - The mode bots: the objective interface ([`Objectives`], [`BotBody`]) and the king-of-the-hill, capture-the-flag
//!   and free-for-all orders ([`koth_order`], [`ctf_order`], [`ffa_order`], [`mode_order`], [`mode_orders`]), plus the
//!   headless [`ScriptedSim`]. `regia_framework::rules::bots` hooks them to the match rules. The book page is
//!   "Deterministic squads and mode bots" (`book/agents/det-ai-squads.md`).
//!
//! Gap AI-7 adds [`UtilityCatalogue`]: a game's own options and inputs as data (`.detutility.ron`, format
//! `ai-det-utility` v1), scored by index ([`score_idx`], [`score_options_idx`], [`ArgMaxIdx`]); DO27's
//! catalogue is its preset ([`UtilityCatalogue::do27_preset`]) with the same bits.

mod scheduler;
mod utility;

mod influence;
mod los;
mod perception;

mod aim;
mod difficulty;
mod tactics;

mod combat;
mod motor;
mod view;

mod mode_bots;
mod wingman;

pub use influence::{
    CELL_DECAY, DECAY_CYCLE_CELLS, INFLUENCE_CELL_M, InfluenceGrid, STAMP_RADIUS_CELLS, flank_bearing,
    line_of_fire, los_clear,
};
pub use los::{
    HeightfieldLos, LOS_CLEARANCE_M, LOS_STEP_M, LineOfSight, LosFn, MouthWall, OpenField,
    heightfield_los_clear,
};
pub use perception::{
    KnownEnemy, LIT_DETECT_BONUS_M, LitContact, PerceptEnemy, PerceptUnit, PerceptionParams, RADAR_RANGE_M,
    SightSource, Sighting, SightingsBoard, ThreatEstimate,
};

pub use aim::{
    AimError, AmmoSlotView, AmmoView, FireIntent, FireTarget, MORTAR_MIN_RANGE_M, ShooterView, Stance,
    WeaponRole, aim_error, ballistic_aim, ballistic_angle, choose_slot, choose_slot_with, fire_gate,
    lead_point, nearest_in_band, seeker_can_lock,
};
pub use difficulty::{
    AiLevel, AimParams, CoordinationMask, DifficultyProfile, RepertoireMask, ScrapParams, Thresholds,
};
pub use tactics::{Decision, Hysteresis, bias, catalogue, decide, force_ratio_norm};

pub use combat::{
    ALLY_KEEPOUT_M, FALLBACK_BAND_EDGE_M, FALLBACK_PROJECTILE_SPEED, FINISH_BONUS, LOW_AMMO_PCT,
    LOW_HULL_PCT, Target, TeamTargets, acquire_range, ally_keepout_m, ammo_pct, fire_intent,
    fire_intent_from, nearest_enemy, reup_needed, reup_point, run as combat_run, run_from as combat_run_from,
    separate_from_allies, target_in_weapon_band, target_priority, target_priority_from, unit_output,
    unit_output_from,
};
pub use motor::{
    ARRIVE_DIST_M, PARK_EPS_M, Pursuit, STEER_DEADBAND, STEER_GAIN, arrive, arrive_moving, bearing_to,
    energy_throttle, evade, formation_slot, kite, orbit, pursue, signed_angle_diff, steer_to,
    throttle_arrive,
};
pub use view::{
    AiUnitOutput, AiUnitView, AiWorld, CoordState, DEFAULT_MAX_HULL, DEFAULT_RADIUS_M, DEFAULT_TARGET_VALUE,
    DEFAULT_TEAM_SLOTS, HoverView, ReupRole, SteerCommand,
};

pub use mode_bots::{
    BotBody, CtfRole, ESCORT_STANDOFF_M, EnemySight, FlagView, HILL_SLOT_RADIUS_M, HUNT_KEEPOUT_M,
    HUNT_SPACING_M, HillView, KothIntent, ModeKind, ModeOrder, Objectives, Pickup, ROTATE_FALLBACK_M,
    SIM_DAMAGE_PER_STEP, SIM_ENGAGE_M, SIM_STEP_M, ScriptedSim, SimBot, acquire_range_m, aim_for,
    assign_ctf_roles, ctf_order, ffa_order, hit_chance, koth_order, mode_order, mode_orders, pickup_radius_m,
    reaction_ticks, retreat_hull_pct,
};
pub use wingman::{
    ADVANCE_LEAD_M, CRITICAL_HULL_PCT, CalloutId, CommandedUnit, FireGate, SteerGoal, TeamDirective,
    WINGMAN_ARRIVE_M, WINGMAN_CLOSE_SPACING_M, WINGMAN_COLUMN_GAP_M, WINGMAN_LANE_CLEAR_M, WINGMAN_LEVEL_M,
    WINGMAN_LEVEL_REACH_M, WINGMAN_LEVEL_STEP_M, WINGMAN_LOOSE_RADIUS_M, WINGMAN_RETREAT_STEP_M,
    WINGMAN_SPACING_M, WingmanCommand, WingmanFormation, WingmanOrder, WingmanPlan, ammo_goal,
    apply_commands, assign_formation_slots, commit_bias, drive, drive_from, fire_blocked,
    formation_is_spaced, formation_slots, level_formation_anchor, plan, repair_goal, report, route,
    run_commanded, slot_clears_leader_lane, spacing_error, stance_from_code, wingman_slot,
};

pub use scheduler::{
    AiAgent, AiCore, AiDecisionRow, AiTickReport, ArgMax, ArgMaxIdx, DecisionPolicy, score_options,
    score_options_idx,
};
pub use utility::{
    AiOption, Consideration, ConsiderationId, CurveKind, Inputs, OptionId, ResponseCurve, score,
};

mod roster;
pub use roster::{
    RosterError, RosterSettings, RosterStats, SlotIndex, SlotIndexStats, TeamRosters, team_group_mask,
};

pub mod targets;
pub use targets::{
    AiRules, AiRulesError, EXHAUSTIVE_RULES_VERSION, LATEST_AI_RULES_VERSION, MAX_TARGET_RINGS, PickKind,
    RINGED_RULES_VERSION, TargetHost, TargetPick, TargetPickReport, TargetRule, TargetSelector, TargetUnit,
    best_target,
};

mod perception_index;
pub use perception_index::{
    PERCEPTION_INDEX_MAX_TEAMS, PerceptionIndex, PerceptionIndexSettings, PerceptionIndexStats,
};

mod world_view;
pub use world_view::{AiTickWorld, AiUnitFacts, AiWorldView};

mod cadence;
pub use cadence::{
    AiCadence, CadenceChannel, CadenceError, CadenceReport, CadenceTier, ChannelId, DECIDE_CHANNEL,
    MAX_CADENCE_CHANNELS, MAX_CADENCE_TIERS, PhaseRule, QueuedUnit, SIGHT_CHANNEL,
};

mod utility_catalogue;
pub use utility_catalogue::{
    ConsiderationExplain, ConsiderationFile, CurveFile, DET_UTILITY_FORMAT, DET_UTILITY_FORMAT_VERSION,
    DET_UTILITY_SUFFIX, DET_UTILITY_TYPE_PATH, DetUtilityError, DetUtilityFile, IdxConsideration, InputDef,
    InputIdx, InputValues, MAX_CATALOGUE_INPUTS, MAX_CATALOGUE_NAME_BYTES, MAX_CATALOGUE_OPTIONS,
    MAX_CATALOGUE_TEXT_BYTES, MAX_OPTION_CONSIDERATIONS, OptionDef, OptionExplain, OptionFile, OptionIdx,
    OptionMask, UtilityCatalogue, det_utility_migration_sources, migrate_det_utility_ron, score_idx,
};

// ---------------------------------------------------------------------------
// Determinism services: AiRng, AiSchedule
// ---------------------------------------------------------------------------

/// Seeded xorshift64, reseeded per unit per stream from sim state only.
/// Never wall-clock, never thread-order.
#[derive(Debug, Clone, Copy)]
pub struct AiRng {
    state: u64,
}

impl AiRng {
    /// A generator seeded with `seed`; a zero seed is replaced by a fixed non-zero one.
    pub fn new(seed: u64) -> Self {
        AiRng {
            state: if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed },
        }
    }

    /// Mix two seeds into a non-zero stream seed (never 0).
    pub fn mix(a: u64, b: u64) -> u64 {
        let mut x = a ^ b.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        x ^= x >> 30;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;
        if x == 0 { 0x9E37_79B9 } else { x }
    }

    /// `unit_stream_seed = mix(world_seed, entity_id)`.
    pub fn unit_stream(world_seed: u64, entity_id: u32) -> Self {
        AiRng::new(Self::mix(world_seed, entity_id as u64))
    }

    /// The unit's stream for one tick: `mix(world_seed ^ tick * 0x9E37_79B9_7F4A_7C15, entity_id)`.
    ///
    /// Each tick of each unit draws from its own stream, so a draw on one tick never shifts the next tick's
    /// numbers and the order units are visited in cannot matter. [`AiCore::decide`] hands this stream to the
    /// scorer.
    pub fn tick_stream(world_seed: u64, tick: u64, entity_id: u32) -> Self {
        AiRng::new(Self::mix(
            world_seed ^ tick.wrapping_mul(0x9E37_79B9_7F4A_7C15),
            u64::from(entity_id),
        ))
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Uniform in [0, 1).
    pub fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Uniform integer in `[min, max]` inclusive.
    pub fn range(&mut self, min: i64, max: i64) -> i64 {
        let span = (max - min + 1).max(1) as u64;
        min + (self.next_u64() % span) as i64
    }
}

/// The deterministic work schedule. Cadences are
/// constants; the 0.5 ms figure is a *measured* ceiling, not a
/// wall-clock throttle here.
///
/// These are Direct Order 27's cadences; [`AiCadence`] is the same schedule as a host setting.
#[derive(Debug, Clone, Copy)]
pub struct AiSchedule;

impl AiSchedule {
    /// Tactic re-pick cadence (≈167 ms at 60 Hz).
    pub const DECISION_TICKS: u32 = 10;
    /// Perception sighting refresh cadence.
    pub const SIGHT_TICKS: u32 = 6;
    /// Measured budget ceiling for 60 units (release build; a benchmark checks it, never a test).
    pub const BUDGET_MS_PER_60_UNITS: f32 = 0.5;

    /// The tick (mod [`Self::DECISION_TICKS`]) on which `entity_id` re-scores.
    #[inline]
    pub fn decision_phase(entity_id: u32) -> u32 {
        entity_id % Self::DECISION_TICKS
    }

    /// The tick (mod [`Self::SIGHT_TICKS`]) on which `entity_id` refreshes its sightings.
    #[inline]
    pub fn sight_phase(entity_id: u32) -> u32 {
        entity_id % Self::SIGHT_TICKS
    }

    /// True on the tick a unit re-scores its options (per-unit phase spreads
    /// the O(options) scoring so all 60 units never re-score on one tick).
    #[inline]
    pub fn due_to_decide(tick: u64, entity_id: u32) -> bool {
        tick % Self::DECISION_TICKS as u64 == Self::decision_phase(entity_id) as u64
    }
}

#[cfg(test)]
mod pipeline_tests {
    // Ported from DO27's `pipeline_tests` (the three tests of the S-09 types; the wingman test is plan row S-13's).
    use super::*;

    #[test]
    fn rng_is_deterministic_and_mix_is_never_zero() {
        let mut a = AiRng::new(42);
        let mut b = AiRng::new(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        for seed in 0u64..10 {
            assert_ne!(AiRng::mix(seed, 1), 0);
            assert_ne!(AiRng::mix(seed, 0), 0);
        }
        let mut r = AiRng::new(7);
        let f = r.next_f32();
        assert!((0.0..1.0).contains(&f));
        let n = r.range(0, 5);
        assert!((0..=5).contains(&n));
    }

    #[test]
    fn unit_streams_are_distinct_per_entity() {
        let s1 = AiRng::unit_stream(0xD027, 1).state;
        let s2 = AiRng::unit_stream(0xD027, 2).state;
        assert_ne!(s1, s2);
    }

    #[test]
    fn schedule_phases_spread_across_units() {
        assert_eq!(AiSchedule::decision_phase(0), 0);
        assert_eq!(AiSchedule::decision_phase(11), 1);
        assert!(AiSchedule::due_to_decide(0, 0));
        assert!(!AiSchedule::due_to_decide(0, 1));
        assert!(AiSchedule::due_to_decide(1, 1));
    }

    #[test]
    fn the_default_cadence_is_the_schedule() {
        let cadence = AiCadence::default();
        for tick in 0..40u64 {
            for id in 0..64u32 {
                assert_eq!(
                    cadence.due_to_decide(tick, id),
                    AiSchedule::due_to_decide(tick, id)
                );
                assert_eq!(cadence.sight_phase(id), AiSchedule::sight_phase(id));
                let next = cadence.next_decision_tick(tick, id);
                assert!(next >= tick && next < tick + 10 && cadence.due_to_decide(next, id));
            }
        }
        assert_eq!(cadence.next_decision_tick(u64::MAX, 9), u64::MAX);
    }
}
