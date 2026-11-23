// SPDX-License-Identifier: MPL-2.0
//! What an agent perceives (`docs/specs/22-tier2.md` § T2.2, ADR-2565): [`PerceptionPlugin`] turns
//! the predicates of [`crate::perception`] into a per-agent memory.
//!
//! Each fixed step, every agent with a [`Perception`] gets a [`Perceived`] component holding its
//! stimuli: which entity, by which [`Sense`], where it was last known to be, on which tick and how
//! strongly. Three senses feed it:
//!
//! - **Sight** tests each `(agent, target)` pair round-robin: a new sweep starts every
//!   [`PerceptionSettings::sight_interval_ticks`] and at most
//!   [`PerceptionSettings::sight_checks_per_tick`] pairs run per fixed step. A pair is the sight
//!   cone of [`crate::perception_hysteresis::sight_holds`] (the acquire cone, or the wider lose
//!   cone for a target already in memory) and, inside it, a line-of-sight ray through
//!   [`regia_physics::WorldQueries`]. Targets are the entities with a [`Perceivable`].
//! - **Hearing** reads [`HearingEvent`] messages through [`can_hear`].
//! - **Damage** reads [`DamageEvent`] messages: the attacker is registered whatever the cone says.
//!
//! The [`Attitudes`] table (team x team to [`Attitude`]) says who counts; [`Perceived::detect`] is
//! the per-agent filter over it. Damage ignores the filter: being shot is news even from a friend.
//! Touch is contact inside [`crate::perception::Perception::touch_radius`] (no cone, no line of
//! sight). A [`Sense::Custom`] id is a game callback registered on [`PerceptionSenses`].
//!
//! Each stimulus keeps a confidence in `[0, 1]`. A per-sense half-life of `None` (the default)
//! leaves confidence at 1 and the last known position where it was sensed, so memory still drops
//! only at [`Perceived::max_age_ticks`]. A set half-life multiplies confidence each unseen tick and
//! steps the last known position by the last known velocity. [`Perceived::target`] weighs confidence.
//!
//! An agent with [`crate::perception::Perception::share_knowledge`] writes its stimuli into
//! [`TeamKnowledge`] (its attitude-group team, and its squad when it has an
//! [`crate::squad::AiSquadMember`]) and reads what the others wrote. Sight can also require a
//! detection meter: [`SightSamples`] rays over the target's bounds, times a [`Visibility`] factor
//! and the cone falloff, accumulated until [`PerceptionSettings::detection_threshold`]. A threshold
//! of 0 acquires on the first clear look, which is today's behavior.
//!
//! A stimulus not sensed for [`Perceived::max_age_ticks`] is forgotten, and at most
//! [`Perceived::capacity`] are kept. [`PerceptionGained`] and [`PerceptionLost`] report the changes.
//! The current target goes to [`Perceived::target`] and, through [`Perceived::target_key`], to the
//! agent's [`Blackboard`], so behavior trees and state trees read it like any other key.
//!
//! # Determinism
//!
//! Everything advances by fixed steps. Agents and sight targets are visited in spawn (arrival)
//! order (ADR-2396, ADR-1347). Stimuli are kept sorted by source and sense, and the ticks are counted
//! here, so two runs with the same spawn order and inputs produce identical lists (no hash
//! iteration, no wall clock). The sight sweep resumes on the observer it stopped on, not by a
//! binary search of entity ids, so a reused entity index cannot skip a perceiver.

use std::collections::{BTreeMap, VecDeque};

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::ecs::entity::Entities;
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_ecs::ecs::system::SystemParam;
use regia_framework::rules::TeamId;
use regia_physics::{PhysicsLayers, PhysicsStep, PhysicsWorld, QueryFilter, WorldQueries};
#[cfg(feature = "debug_draw")]
use regia_render::gizmos::{AiPerceptionGizmos, Gizmos};
use regia_scene::GlobalTransform;
use regia_scene::Transform;
use regia_scene::layers::LayerParked;

use crate::AiStep;
use crate::blackboard::{BbValue, Blackboard};
use crate::perception::{HearingEvent, Perception, can_hear};
use crate::perception_hysteresis::{
    SightRanges, age_position, cone_falloff, confidence_after_tick, detection_gain, meter_acquires,
    sample_offset, sight_holds,
};
use crate::squad::AiSquadMember;

/// The most hearing or damage messages waiting for a fixed step; the oldest go first past this.
const MAX_QUEUED_MESSAGES: usize = 4096;

/// Edge of a hearing cell, in metres. A sound tests the cells its radius reaches.
const HEARING_CELL_METRES: f32 = 8.0;

/// A current target's score is multiplied by this when it competes, so two similar targets do not
/// trade places every step.
const CURRENT_TARGET_STICKINESS: f32 = 1.25;

/// A way of perceiving something.
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[reflect(Default)]
pub enum Sense {
    /// Inside the sight cone with a clear line of sight.
    #[default]
    Sight,
    /// A [`HearingEvent`] within range.
    Hearing,
    /// A [`DamageEvent`] naming this agent as the victim.
    Damage,
    /// Contact or proximity inside [`crate::perception::Perception::touch_radius`]. No cone and no
    /// line of sight.
    Touch,
    /// A game callback registered with [`PerceptionSenses`]. The `u16` is the id `register` returned.
    Custom(u16),
}

impl Sense {
    /// The lowercase name the agent protocol shows.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Sight => "sight",
            Self::Hearing => "hearing",
            Self::Damage => "damage",
            Self::Touch => "touch",
            Self::Custom(_) => "custom",
        }
    }
}

/// How one team regards another.
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[reflect(Default)]
pub enum Attitude {
    /// An enemy: perceived, and eligible as the current target.
    Hostile,
    /// Neither: perceived, but a target only after it deals damage.
    #[default]
    Neutral,
    /// An ally: filtered out by the default [`AttitudeFilter`], and never a target.
    Friendly,
}

impl Attitude {
    /// The lowercase name the agent protocol shows.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Hostile => "hostile",
            Self::Neutral => "neutral",
            Self::Friendly => "friendly",
        }
    }

    /// Parses [`Attitude::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "hostile" => Some(Self::Hostile),
            "neutral" => Some(Self::Neutral),
            "friendly" => Some(Self::Friendly),
            _ => None,
        }
    }
}

/// The team x team attitude table. A pair with no entry reads [`Attitudes::same_team`] when both
/// teams are the same and [`Attitudes::other_team`] when they differ. Entries are directional:
/// team 1 may fear team 2 while team 2 ignores team 1.
#[derive(Resource, Reflect, Debug, Clone, PartialEq, Eq)]
#[reflect(Resource, Default)]
pub struct Attitudes {
    /// A team's attitude to itself, without an entry. Default [`Attitude::Friendly`].
    pub same_team: Attitude,
    /// A team's attitude to any other team, without an entry. Default [`Attitude::Hostile`].
    pub other_team: Attitude,
    /// The explicit entries, keyed `from * 256 + to`; read and write them with [`Attitudes::get`]
    /// and [`Attitudes::set`].
    pub entries: BTreeMap<u16, Attitude>,
}

impl Default for Attitudes {
    fn default() -> Self {
        Self {
            same_team: Attitude::Friendly,
            other_team: Attitude::Hostile,
            entries: BTreeMap::new(),
        }
    }
}

impl Attitudes {
    const fn key(from: u8, to: u8) -> u16 {
        ((from as u16) << 8) | to as u16
    }

    /// How team `from` regards team `to`.
    #[must_use]
    pub fn get(&self, from: u8, to: u8) -> Attitude {
        self.entries
            .get(&Self::key(from, to))
            .copied()
            .unwrap_or(if from == to {
                self.same_team
            } else {
                self.other_team
            })
    }

    /// Sets how team `from` regards team `to` and returns what it was. The other direction is
    /// unchanged.
    pub fn set(&mut self, from: u8, to: u8, attitude: Attitude) -> Attitude {
        let previous = self.get(from, to);
        self.entries.insert(Self::key(from, to), attitude);
        previous
    }
}

/// Which attitudes one agent's sight and hearing register. Damage is not filtered.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq)]
#[reflect(Default)]
pub struct AttitudeFilter {
    /// Register hostile sources. Default on.
    pub hostile: bool,
    /// Register neutral sources. Default on.
    pub neutral: bool,
    /// Register friendly sources. Default off.
    pub friendly: bool,
}

impl Default for AttitudeFilter {
    fn default() -> Self {
        Self {
            hostile: true,
            neutral: true,
            friendly: false,
        }
    }
}

impl AttitudeFilter {
    /// Whether `attitude` passes.
    #[must_use]
    pub const fn allows(&self, attitude: Attitude) -> bool {
        match attitude {
            Attitude::Hostile => self.hostile,
            Attitude::Neutral => self.neutral,
            Attitude::Friendly => self.friendly,
        }
    }
}

/// An entity agents can see. Sight only tests entities that carry one, so what is worth looking at
/// is the game's choice and the pair count stays the game's to bound.
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default)]
pub struct Perceivable {
    /// Team affiliation, looked up in [`Attitudes`].
    pub team: u8,
    /// Height above the entity's origin that lines of sight aim at (metres).
    pub aim_height: f32,
    /// Half extents of the body sight samples are spread across (metres). Zero (the default) aims
    /// every sample at the center, which is one ray.
    pub bounds_half_extents: Vec3,
}

impl Default for Perceivable {
    fn default() -> Self {
        Self {
            team: 0,
            aim_height: 1.0,
            bounds_half_extents: Vec3::ZERO,
        }
    }
}

/// How hidden a target is, from the game: light, stance, camouflage. Absent means fully visible.
/// The factor multiplies the detection meter's gain. A non-finite factor counts as 0.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component, Default)]
pub struct Visibility {
    /// Multiplier in `[0, 1]`. `1` is fully lit, `0` is unseen by sight.
    pub factor: f32,
}

impl Default for Visibility {
    fn default() -> Self {
        Self { factor: 1.0 }
    }
}

/// How many sight rays an observer casts across a target's [`Perceivable::bounds_half_extents`].
/// Absent, or a count of 0 or 1, is the single center ray.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct SightSamples {
    /// Sample count. Capped at 64. The rays of one target are cast together or not at all.
    pub count: u8,
}

impl Default for SightSamples {
    fn default() -> Self {
        Self { count: 1 }
    }
}

/// Evidence accumulated toward seeing one target. Stored on [`Perceived`] so the agent protocol
/// can read the meter. An empty list takes no heap.
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Default)]
pub struct DetectionSample {
    /// The target the evidence is about.
    pub target: Entity,
    /// Accumulated gain. Acquisition happens at [`PerceptionSettings::detection_threshold`].
    pub evidence: f32,
}

impl Default for DetectionSample {
    fn default() -> Self {
        Self {
            target: Entity::PLACEHOLDER,
            evidence: 0.0,
        }
    }
}

/// One thing one agent perceived by one sense.
#[derive(Reflect, Debug, Clone, PartialEq)]
#[reflect(Default)]
pub struct Stimulus {
    /// What was sensed; [`Entity::PLACEHOLDER`] for an anonymous sound or wound (all anonymous
    /// stimuli of one sense share one entry, the latest).
    pub entity: Entity,
    /// By which sense.
    pub sense: Sense,
    /// The source's team when sensed; `None` when an attacker has no [`Perceivable`] or
    /// [`Perception`] to read it from (then it counts as hostile).
    pub team: Option<u8>,
    /// Where the source was when last sensed.
    pub last_known_position: Vec3,
    /// The [`PerceptionStats::tick`] it was last sensed on.
    pub last_sensed_tick: u64,
    /// How strongly, in `[0, 1]`: nearness inside the sight or hearing range, or the damage over
    /// [`PerceptionSettings::damage_full_strength`].
    pub strength: f32,
    /// How sure the memory is, in `[0, 1]`. A fresh sense sets it to 1. It decays by the sense's
    /// half-life while the target is unseen. The default is 1, so a half-life of `None` leaves
    /// target scores where they were.
    pub confidence: f32,
    /// The last sensed step of the source, in metres per fixed step. With a half-life set, the
    /// last known position advances by this each unseen tick. The default is zero, and a half-life
    /// of `None` does not move the position.
    pub last_known_velocity: Vec3,
}

impl Default for Stimulus {
    fn default() -> Self {
        Self {
            entity: Entity::PLACEHOLDER,
            sense: Sense::Sight,
            team: None,
            last_known_position: Vec3::ZERO,
            last_sensed_tick: 0,
            strength: 0.0,
            confidence: 1.0,
            last_known_velocity: Vec3::ZERO,
        }
    }
}

/// What one agent perceives and remembers. [`PerceptionPlugin`] adds it to any entity with a
/// [`Perception`] that lacks one; add it yourself to change the settings from the start.
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default)]
pub struct Perceived {
    /// The remembered stimuli, sorted by source and sense.
    pub stimuli: Vec<Stimulus>,
    /// The current target, or [`Entity::PLACEHOLDER`]: the strongest fresh stimulus of a hostile
    /// source, or of a neutral one that dealt damage.
    pub target: Entity,
    /// A stimulus not sensed for more than this many fixed steps is forgotten.
    pub max_age_ticks: u64,
    /// The most stimuli kept; past it the stalest goes first.
    pub capacity: u32,
    /// Which attitudes sight and hearing register.
    pub detect: AttitudeFilter,
    /// Height above the entity's origin that lines of sight start from (metres).
    pub eye_height: f32,
    /// The blackboard key of the current target (`BbValue::Entity`). `<key>_position` holds its last
    /// known position and `<key>_known` is `true` while there is a target. Empty writes nothing.
    pub target_key: String,
    /// Sight evidence that has not yet become a stimulus, and the meter of a target already known.
    /// Empty until a positive [`PerceptionSettings::detection_threshold`] starts filling it.
    pub meter: Vec<DetectionSample>,
}

impl Default for Perceived {
    fn default() -> Self {
        Self {
            stimuli: Vec::new(),
            target: Entity::PLACEHOLDER,
            max_age_ticks: 300,
            capacity: 32,
            detect: AttitudeFilter::default(),
            eye_height: 1.5,
            target_key: DEFAULT_TARGET_KEY.to_string(),
            meter: Vec::new(),
        }
    }
}

/// The default [`Perceived::target_key`].
pub const DEFAULT_TARGET_KEY: &str = "perception_target";

impl Perceived {
    /// The stimulus of `entity` by `sense`, if remembered.
    #[must_use]
    pub fn get(&self, entity: Entity, sense: Sense) -> Option<&Stimulus> {
        self.stimuli
            .binary_search_by_key(&stimulus_key(entity, sense), |s| stimulus_key(s.entity, s.sense))
            .ok()
            .map(|index| &self.stimuli[index])
    }

    /// Whether `entity` is remembered by any sense.
    #[must_use]
    pub fn knows(&self, entity: Entity) -> bool {
        self.stimuli.iter().any(|s| s.entity == entity)
    }
}

/// Global perception settings.
#[derive(Resource, Reflect, Debug, Clone, PartialEq)]
#[reflect(Resource, Default)]
pub struct PerceptionSettings {
    /// A new sight sweep over every `(agent, target)` pair starts at most this often, in fixed steps.
    /// `6` is 10 Hz on a 60 Hz step. A sweep the budget stretches past it starts the next one late.
    pub sight_interval_ticks: u32,
    /// The most `(agent, target)` pairs sight visits in one fixed step (the per-frame budget of the
    /// simulation). `0` stops sight.
    pub sight_checks_per_tick: u32,
    /// The damage amount that reads as strength 1.
    pub damage_full_strength: f32,
    /// Ticks for sight confidence to fall by half while the target is unseen. `None` or `Some(0)`
    /// does not decay (the memory drops at [`Perceived::max_age_ticks`] instead).
    pub sight_confidence_half_life: Option<u32>,
    /// Hearing confidence half-life. `None` or `Some(0)` does not decay.
    pub hearing_confidence_half_life: Option<u32>,
    /// Damage confidence half-life. `None` or `Some(0)` does not decay.
    pub damage_confidence_half_life: Option<u32>,
    /// Touch confidence half-life. `None` or `Some(0)` does not decay.
    pub touch_confidence_half_life: Option<u32>,
    /// Sight evidence required before a new target is acquired. `0` (the default) acquires on the
    /// first clear look. A remembered target refreshes without refilling the meter.
    pub detection_threshold: f32,
    /// Fixed steps between touch passes. `1` tests every step. `0` is treated as 1.
    pub touch_interval_ticks: u32,
}

impl Default for PerceptionSettings {
    fn default() -> Self {
        Self {
            sight_interval_ticks: 6,
            sight_checks_per_tick: 64,
            damage_full_strength: 25.0,
            sight_confidence_half_life: None,
            hearing_confidence_half_life: None,
            damage_confidence_half_life: None,
            touch_confidence_half_life: None,
            detection_threshold: 0.0,
            touch_interval_ticks: 1,
        }
    }
}

/// Counters for profiling and the agent protocol. Deterministic: they count fixed-step work.
#[derive(Resource, Reflect, Debug, Clone, Default, PartialEq, Eq)]
#[reflect(Resource, Default)]
pub struct PerceptionStats {
    /// Fixed steps run so far (the clock of [`Stimulus::last_sensed_tick`]).
    pub tick: u64,
    /// Pairs sight visited in the last fixed step.
    pub sight_checks_last_tick: u32,
    /// Line-of-sight rays cast in the last fixed step (pairs that passed the attitude filter and
    /// the cone).
    pub sight_rays_last_tick: u32,
    /// Pairs sight visited in all.
    pub sight_checks_total: u64,
    /// Full sweeps finished.
    pub sweeps_completed: u64,
    /// Hearing messages taken in.
    pub hearing_events: u64,
    /// Damage messages taken in.
    pub damage_events: u64,
    /// Messages lost because no fixed step read them in time (the queue holds 4096 of each kind).
    pub messages_dropped: u64,
    /// World poses sight read in the last fixed step. A budgeted step reads the pairs it tests,
    /// not every observer and candidate.
    pub sight_pose_reads_last_tick: u32,
    /// Agents whose hearing radius was tested in the last fixed step (`can_hear` calls, after the
    /// attitude filter). A sound tests the cells it reaches, not every agent.
    pub hearing_tests_last_tick: u32,
}

/// `victim` was hurt: its [`Perceived`] registers `attacker` by [`Sense::Damage`], in or out of the
/// sight cone and whatever the attitude.
#[derive(Message, Reflect, Debug, Clone, PartialEq)]
#[reflect(Default)]
pub struct DamageEvent {
    /// Who was hurt. A victim without a [`Perceived`] is ignored.
    pub victim: Entity,
    /// Who did it, or [`Entity::PLACEHOLDER`] when nobody did (a fall, the weather).
    pub attacker: Entity,
    /// Where the attacker is known to be: its position or, for a shot, its muzzle.
    pub position: Vec3,
    /// How much it hurt (game units). Negative or non-finite reads as 0.
    pub amount: f32,
}

impl Default for DamageEvent {
    fn default() -> Self {
        Self {
            victim: Entity::PLACEHOLDER,
            attacker: Entity::PLACEHOLDER,
            position: Vec3::ZERO,
            amount: 0.0,
        }
    }
}

/// An agent started perceiving `source` by `sense` (it had no such stimulus).
#[derive(Message, Reflect, Debug, Clone, PartialEq)]
pub struct PerceptionGained {
    /// The perceiving agent.
    pub agent: Entity,
    /// What it perceives.
    pub source: Entity,
    /// By which sense.
    pub sense: Sense,
    /// Where.
    pub position: Vec3,
    /// How strongly.
    pub strength: f32,
}

/// Why a stimulus left an agent's memory.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LostReason {
    /// Not sensed for [`Perceived::max_age_ticks`].
    Expired,
    /// Pushed out by [`Perceived::capacity`].
    Evicted,
    /// The source no longer exists.
    Despawned,
}

/// An agent forgot `source` by `sense`.
#[derive(Message, Reflect, Debug, Clone, PartialEq)]
pub struct PerceptionLost {
    /// The agent.
    pub agent: Entity,
    /// What it forgot.
    pub source: Entity,
    /// Which sense's memory.
    pub sense: Sense,
    /// Why.
    pub reason: LostReason,
}

/// One fact an agent wrote into [`TeamKnowledge`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SharedStimulus {
    /// The sensed entity.
    pub entity: Entity,
    /// Which sense produced it.
    pub sense: Sense,
    /// The source's team when it was sensed.
    pub team: Option<u8>,
    /// Last known position.
    pub position: Vec3,
    /// Last known velocity, metres per fixed step.
    pub velocity: Vec3,
    /// Confidence in `[0, 1]` at the moment it was shared.
    pub confidence: f32,
    /// Strength in `[0, 1]`.
    pub strength: f32,
    /// The reporter's [`Stimulus::last_sensed_tick`]. The receiver stamps its own memory with the
    /// current tick so the copy is not decayed on the tick it arrived.
    pub sensed_tick: u64,
    /// The agent that wrote the fact. A reader skips its own reports.
    pub reporter: Entity,
}

/// Facts shared by attitude-group team and by squad. Empty maps take no heap. Not reflected: the
/// lists are runtime memory, rebuilt every step from the agents that opt in.
#[derive(Resource, Debug, Clone, PartialEq, Default)]
pub struct TeamKnowledge {
    /// Half-life of a shared fact that nobody refreshed, in fixed steps. `None` or `Some(0)` keeps
    /// the fact until a writer overwrites it.
    pub half_life_ticks: Option<u32>,
    teams: BTreeMap<u8, Vec<SharedStimulus>>,
    squads: BTreeMap<u64, Vec<SharedStimulus>>,
}

/// The most facts one team or one squad keeps. Past it, the stalest sensed tick goes.
const SHARED_CAP: usize = 32;

impl TeamKnowledge {
    /// Facts team `team` has shared. An unknown team is an empty slice.
    #[must_use]
    pub fn team(&self, team: u8) -> &[SharedStimulus] {
        self.teams.get(&team).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Facts squad `squad` has shared. An unknown squad is an empty slice.
    #[must_use]
    pub fn squad(&self, squad: Entity) -> &[SharedStimulus] {
        self.squads
            .get(&squad.to_bits())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Every shared fact, teams then squads.
    pub fn iter_facts(&self) -> impl Iterator<Item = &SharedStimulus> {
        self.teams
            .values()
            .flatten()
            .chain(self.squads.values().flatten())
    }

    fn decay(&mut self) {
        let half = self.half_life_ticks.filter(|half| *half > 0);
        if half.is_none() {
            return;
        }
        for list in self.teams.values_mut().chain(self.squads.values_mut()) {
            for fact in list.iter_mut() {
                fact.confidence = confidence_after_tick(fact.confidence, half);
                fact.position = age_position(fact.position, fact.velocity, half);
            }
            list.retain(|fact| fact.confidence > 0.02);
        }
    }

    fn upsert(list: &mut Vec<SharedStimulus>, fact: SharedStimulus) {
        if let Some(slot) = list
            .iter_mut()
            .find(|held| held.entity == fact.entity && held.sense == fact.sense)
        {
            *slot = fact;
            return;
        }
        if list.len() >= SHARED_CAP {
            let mut worst = 0;
            for (index, held) in list.iter().enumerate() {
                if held.sensed_tick < list[worst].sensed_tick {
                    worst = index;
                }
            }
            list.swap_remove(worst);
        }
        list.push(fact);
    }

    fn upsert_team(&mut self, team: u8, fact: SharedStimulus) {
        Self::upsert(self.teams.entry(team).or_default(), fact);
    }

    fn upsert_squad(&mut self, squad: u64, fact: SharedStimulus) {
        Self::upsert(self.squads.entry(squad).or_default(), fact);
    }
}

/// `register` refused the sense.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SenseRegisterError {
    /// The name was empty.
    EmptyName,
    /// That name is already registered.
    DuplicateName,
}

impl SenseRegisterError {
    /// Agent error code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::EmptyName => "empty_sense_name",
            Self::DuplicateName => "duplicate_sense",
        }
    }
}

/// Game callback for [`Sense::Custom`]. `Some(strength)` in `[0, 1]` records the candidate.
/// `None` records nothing. A non-finite strength is ignored.
pub type SenseCallback = fn(&World, Entity, Entity) -> Option<f32>;

struct SenseEntry {
    id: u16,
    name: String,
    range: f32,
    interval_ticks: u32,
    half_life_ticks: Option<u32>,
    callback: SenseCallback,
    next_tick: u64,
}

/// One registered custom sense, without its callback.
#[derive(Debug, Clone, PartialEq)]
pub struct SenseListed<'a> {
    /// The id [`Sense::Custom`] carries.
    pub id: u16,
    /// The name passed to [`PerceptionSenses::register`].
    pub name: &'a str,
    /// Range in metres. A non-positive range does not filter.
    pub range: f32,
    /// Fixed steps between passes.
    pub interval_ticks: u32,
    /// Confidence half-life. `None` does not decay.
    pub half_life_ticks: Option<u32>,
}

/// Names and callbacks for [`Sense::Custom`]. Not reflected: a callback is a function pointer.
/// An empty registry takes no heap, and the perception step ignores it.
#[derive(Resource)]
pub struct PerceptionSenses {
    next_id: u16,
    entries: Vec<SenseEntry>,
}

impl Default for PerceptionSenses {
    fn default() -> Self {
        Self {
            next_id: 1,
            entries: Vec::new(),
        }
    }
}

impl std::fmt::Debug for PerceptionSenses {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PerceptionSenses")
            .field("next_id", &self.next_id)
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl PerceptionSenses {
    /// Whether any custom sense is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many custom senses are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Registers `name`. Ids start at 1. `interval_ticks` of 0 is treated as 1 when scheduling.
    /// The first pass is due on the next perception step.
    pub fn register(
        &mut self,
        name: &str,
        range: f32,
        interval_ticks: u32,
        callback: SenseCallback,
    ) -> Result<u16, SenseRegisterError> {
        if name.is_empty() {
            return Err(SenseRegisterError::EmptyName);
        }
        if self.entries.iter().any(|entry| entry.name == name) {
            return Err(SenseRegisterError::DuplicateName);
        }
        let id = self.next_id;
        if id == 0 {
            return Err(SenseRegisterError::DuplicateName);
        }
        self.next_id = self.next_id.saturating_add(1);
        self.entries.push(SenseEntry {
            id,
            name: name.to_string(),
            range,
            interval_ticks,
            half_life_ticks: None,
            callback,
            next_tick: 0,
        });
        Ok(id)
    }

    /// Sets the confidence half-life of a registered sense. Unknown ids do nothing and return false.
    pub fn set_half_life(&mut self, id: u16, half_life_ticks: Option<u32>) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return false;
        };
        entry.half_life_ticks = half_life_ticks.filter(|half| *half > 0);
        true
    }

    /// The half-life of custom sense `id`, if it is registered and set.
    #[must_use]
    pub fn half_life(&self, id: u16) -> Option<u32> {
        self.entries
            .iter()
            .find(|entry| entry.id == id)
            .and_then(|entry| entry.half_life_ticks)
    }

    /// The registered senses, in id order.
    pub fn iter(&self) -> impl Iterator<Item = SenseListed<'_>> {
        self.entries.iter().map(|entry| SenseListed {
            id: entry.id,
            name: &entry.name,
            range: entry.range,
            interval_ticks: entry.interval_ticks,
            half_life_ticks: entry.half_life_ticks,
        })
    }

    /// The sense named `name`.
    #[must_use]
    pub fn by_name(&self, name: &str) -> Option<SenseListed<'_>> {
        self.iter().find(|entry| entry.name == name)
    }

    fn any_half_life(&self) -> bool {
        self.entries.iter().any(|entry| entry.half_life_ticks.is_some())
    }
}

/// `FixedUpdate` set of [`PerceptionPlugin`]: after `PhysicsStep`, so lines of sight see this step's
/// world, and before `AiStep`, so trees read this step's perception.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PerceptionStep;

/// Installs perception: [`Perceived`] on every agent with a [`Perception`], the senses, the
/// [`Attitudes`] table, and the target write-through. Opt-in (not part of [`crate::AiPlugin`]).
/// Add `regia_physics::PhysicsPlugin` for lines of sight: without a physics world every line is
/// clear, as for [`crate::line_of_sight`]. Touch, custom senses, confidence decay, team knowledge
/// and the detection meter stay off until the game sets them, so the default step matches the
/// three built-in senses.
#[derive(Default)]
pub struct PerceptionPlugin;

impl Plugin for PerceptionPlugin {
    fn build(&self, app: &mut App) {
        // Arrival order of both sides of a sight pair: the visiting order, the score-tie break and
        // eviction follow it instead of the entity id (ids are record-time allocations, ADR-1347).
        regia_ecs::spawn_order::track_spawn_order::<Perception>(app.world_mut());
        regia_ecs::spawn_order::track_spawn_order::<Perceivable>(app.world_mut());
        app.register_type::<Perception>()
            .register_type::<Perceived>()
            .register_type::<Perceivable>()
            .register_type::<Stimulus>()
            .register_type::<Sense>()
            .register_type::<Attitude>()
            .register_type::<AttitudeFilter>()
            .register_type::<Attitudes>()
            .register_type::<PerceptionSettings>()
            .register_type::<PerceptionStats>()
            .register_type::<HearingEvent>()
            .register_type::<DamageEvent>()
            .register_type::<PerceptionGained>()
            .register_type::<PerceptionLost>()
            .register_type::<LostReason>()
            .register_type::<Visibility>()
            .register_type::<SightSamples>()
            .register_type::<DetectionSample>()
            .init_resource::<Attitudes>()
            .init_resource::<PerceptionSettings>()
            .init_resource::<PerceptionStats>()
            .init_resource::<PerceptionIntake>()
            .init_resource::<TeamKnowledge>()
            .init_resource::<PerceptionSenses>()
            .add_message::<HearingEvent>()
            .add_message::<DamageEvent>()
            .add_message::<PerceptionGained>()
            .add_message::<PerceptionLost>()
            .configure_sets(FixedUpdate, PerceptionStep.after(PhysicsStep).before(AiStep))
            .add_systems(
                FixedUpdate,
                (
                    sync_team_id_to_perception,
                    ensure_perceived,
                    perception_tick,
                    custom_senses,
                )
                    .chain()
                    .in_set(PerceptionStep),
            )
            .add_systems(Last, collect_messages);
        #[cfg(feature = "debug_draw")]
        app.add_systems(
            PostUpdate,
            draw_memory_gizmos
                .run_if(regia_render::gizmos::gizmo_group_enabled::<regia_render::gizmos::AiPerceptionGizmos>)
                .after(PostUpdateSet::TransformPropagate),
        );
    }
}

/// Copy [`TeamId`] onto [`Perceivable::team`] when both components are present.
///
/// Runs before sight, so a team change this step is the team the attitude table sees. The
/// observer's [`Perception::team`] is left alone. Team `0` is neutral.
fn sync_team_id_to_perception(mut query: Query<(&TeamId, &mut Perceivable)>) {
    for (team, mut perceivable) in &mut query {
        perceivable.team = team.0;
    }
}

/// Storage order of [`Perceived::stimuli`]: entity id, then sense (what `Perceived::get` binary
/// searches). Ties that matter — the target pick and eviction — are broken on the source's
/// [`Perceivable`] spawn-order key instead (`pick_target`, `evict_one`).
fn stimulus_key(entity: Entity, sense: Sense) -> (u32, u64, Sense) {
    (entity.index_u32(), entity.to_bits(), sense)
}

/// What [`world_pose`] and the ancestor walk of [`line_clear`] read.
type PoseQuery<'w, 's> = Query<
    'w,
    's,
    (
        Option<&'static Transform>,
        Option<&'static GlobalTransform>,
        Option<&'static ChildOf>,
    ),
>;

/// The world position and facing of an entity, from the local `Transform`s up its `ChildOf` chain. The
/// `GlobalTransform` is only propagated once per frame, so reading it would make the answer on the
/// second fixed step of a frame depend on the frame rate; it serves an entity or ancestor with no
/// `Transform`.
fn world_pose(entity: Entity, poses: &PoseQuery<'_, '_>) -> Option<(Vec3, Vec3)> {
    let (transform, global, link) = poses.get(entity).ok()?;
    crate::tick_budget::pose_from_chain(transform, global, link, |e| {
        poses.get(e).unwrap_or((None, None, None))
    })
}

/// Messages waiting for the next fixed step, and where each reader stands. A frame with no fixed
/// step would otherwise lose them: message buffers turn over every frame, and at 240 fps on a
/// 60 Hz step most frames have no step.
#[derive(Resource, Default)]
pub(crate) struct PerceptionIntake {
    hearing_cursor: MessageCursor<HearingEvent>,
    damage_cursor: MessageCursor<DamageEvent>,
    hearing: VecDeque<HearingEvent>,
    damage: VecDeque<DamageEvent>,
    dropped: u64,
}

impl PerceptionIntake {
    fn collect(&mut self, hearing: &Messages<HearingEvent>, damage: &Messages<DamageEvent>) {
        for event in self.hearing_cursor.read(hearing) {
            if self.hearing.len() == MAX_QUEUED_MESSAGES {
                self.hearing.pop_front();
                self.dropped += 1;
            }
            self.hearing.push_back(event.clone());
        }
        for event in self.damage_cursor.read(damage) {
            if self.damage.len() == MAX_QUEUED_MESSAGES {
                self.damage.pop_front();
                self.dropped += 1;
            }
            self.damage.push_back(event.clone());
        }
    }
}

/// `Last`: moves this frame's messages into the queue, for the frames that had no fixed step.
fn collect_messages(
    mut intake: ResMut<PerceptionIntake>,
    hearing: Res<Messages<HearingEvent>>,
    damage: Res<Messages<DamageEvent>>,
) {
    if !hearing.is_empty() || !damage.is_empty() {
        intake.collect(&hearing, &damage);
    }
}

/// Gives every [`Perception`] agent its memory.
fn ensure_perceived(mut commands: Commands, new: Query<Entity, (With<Perception>, Without<Perceived>)>) {
    for entity in &new {
        commands.entity(entity).try_insert(Perceived::default());
    }
}

/// One observer's identity for a sweep. The eye and facing are read when the pair is tested.
#[derive(Clone, Copy)]
struct Observer {
    entity: Entity,
    team: u8,
    half_angle: f32,
    radius: f32,
    /// Raw [`Perception::sight_lose_radius`]. Zero is resolved inside [`sight_holds`].
    lose_radius: f32,
    /// Raw [`Perception::sight_lose_half_angle`]. Zero is resolved inside [`sight_holds`].
    lose_half_angle: f32,
    detect: AttitudeFilter,
    /// [`SightSamples::count`], or 1 when the observer has no component.
    samples: u32,
}

/// One sight candidate's identity for a sweep. The aim point is read when the pair is tested.
#[derive(Clone, Copy)]
struct Candidate {
    entity: Entity,
    team: u8,
    /// The perceivable's spawn-order key, the sort and resume key of the sight sweep.
    key: regia_ecs::spawn_order::SpawnKey,
    /// [`Perceivable::bounds_half_extents`], copied so the ray loop does not re-read the component.
    bounds: Vec3,
    /// [`Visibility::factor`] clamped to `[0, 1]`, or 1 when the target has no component.
    visibility: f32,
}

/// One agent's position for the hearing grid. `posed` is false when the world pose could not be read.
struct HearSample {
    entity: Entity,
    position: Vec3,
    posed: bool,
}

fn hearing_cell(position: Vec3) -> [i32; 3] {
    [
        (position.x / HEARING_CELL_METRES).floor() as i32,
        (position.y / HEARING_CELL_METRES).floor() as i32,
        (position.z / HEARING_CELL_METRES).floor() as i32,
    ]
}

/// Indices of hearing-grid samples whose cells overlap the event sphere, in ascending index order
/// (the agents' spawn order, because samples are stored that way).
fn collect_hearing_hits(
    grid: &BTreeMap<[i32; 3], Vec<u32>>,
    hits: &mut Vec<u32>,
    origin: Vec3,
    radius: f32,
    samples: u32,
) {
    hits.clear();
    if !(radius.is_finite() && radius >= 0.0) || samples == 0 {
        return;
    }
    let reach = Vec3::splat(radius);
    let lo = hearing_cell(origin - reach);
    let hi = hearing_cell(origin + reach);
    // A sphere that covers more cells than there are agents is cheaper as a full scan.
    let span = |a: i32, b: i32| i64::from(b).saturating_sub(i64::from(a)).saturating_add(1);
    let cells = span(lo[0], hi[0])
        .saturating_mul(span(lo[1], hi[1]))
        .saturating_mul(span(lo[2], hi[2]));
    if cells > i64::from(samples) {
        hits.extend(0..samples);
        return;
    }
    for x in lo[0]..=hi[0] {
        for y in lo[1]..=hi[1] {
            for z in lo[2]..=hi[2] {
                if let Some(found) = grid.get(&[x, y, z]) {
                    hits.extend_from_slice(found);
                }
            }
        }
    }
    hits.sort_unstable();
}

/// Where the round-robin stands between fixed steps: the observer entity the sweep stopped on
/// (looked up in this tick's observer list, which is kept in spawn order) and that pair's
/// candidate sort key (the perceivable's spawn-order key, valid because the candidates are
/// re-sorted by it every tick).
#[derive(Default)]
struct Sweep {
    active: bool,
    next_start: u64,
    /// The last pair visited; `None` before the first pair.
    last: Option<(Entity, regia_ecs::spawn_order::SpawnKey)>,
}

/// The reusable buffers and the sweep of [`perception_tick`].
#[derive(Default)]
struct TickScratch {
    agents: Vec<Entity>,
    observers: Vec<Observer>,
    candidates: Vec<Candidate>,
    sweep: Sweep,
    key: String,
    hear_samples: Vec<HearSample>,
    hear_grid: BTreeMap<[i32; 3], Vec<u32>>,
    hear_hits: Vec<u32>,
    /// Set once a hearing grid has been built for the current agent list.
    hear_built: bool,
    /// The next tick touch may run. `0` means the first tick an agent has a radius is due.
    touch_next: u64,
    /// Sight targets dropped this step, so their detection-meter rows go with them.
    expired_sights: Vec<Entity>,
    /// Facts copied out of [`TeamKnowledge`] before they are recorded. Empty until someone shares.
    share_in: Vec<SharedStimulus>,
}

/// Where a stimulus goes this tick.
#[derive(Clone, Copy)]
struct Sensed {
    source: Entity,
    sense: Sense,
    team: Option<u8>,
    position: Vec3,
    strength: f32,
    /// A direct sense forces confidence to 1 and estimates velocity. A shared fact copies both.
    direct: bool,
    confidence: f32,
    velocity: Vec3,
    /// For a shared fact, the reporter's sensed tick. A direct sense leaves this 0.
    reported_tick: u64,
}

/// A direct observation: confidence 1, velocity filled in by [`record`] on a refresh.
fn direct_sense(source: Entity, sense: Sense, team: Option<u8>, position: Vec3, strength: f32) -> Sensed {
    Sensed {
        source,
        sense,
        team,
        position,
        strength,
        direct: true,
        confidence: 1.0,
        velocity: Vec3::ZERO,
        reported_tick: 0,
    }
}

/// Pushes out the stalest stimulus, then the weakest; on a full tie the latest arrival (the
/// [`Perceivable`] spawn-order key decides, not the entity id, ADR-1347).
fn evict_one(
    agent: Entity,
    perceived: &mut Perceived,
    lost: &mut impl FnMut(PerceptionLost),
    key: &dyn Fn(Entity) -> regia_ecs::spawn_order::SpawnKey,
) {
    let mut evict = 0;
    for (index, s) in perceived.stimuli.iter().enumerate() {
        let best = &perceived.stimuli[evict];
        let worse = s.last_sensed_tick < best.last_sensed_tick
            || (s.last_sensed_tick == best.last_sensed_tick && s.strength.total_cmp(&best.strength).is_lt())
            || (s.last_sensed_tick == best.last_sensed_tick
                && s.strength == best.strength
                && key(s.entity) > key(best.entity));
        if worse {
            evict = index;
        }
    }
    if evict < perceived.stimuli.len() {
        let out = perceived.stimuli.remove(evict);
        lost(PerceptionLost {
            agent,
            source: out.entity,
            sense: out.sense,
            reason: LostReason::Evicted,
        });
    }
}

fn finite_unit(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn finite_velocity(value: Vec3) -> Vec3 {
    if value.is_finite() { value } else { Vec3::ZERO }
}

/// The half-life that applies to `sense`. `None` and `Some(0)` mean "do not decay".
fn half_life_of(
    settings: &PerceptionSettings,
    senses: Option<&PerceptionSenses>,
    sense: Sense,
) -> Option<u32> {
    let raw = match sense {
        Sense::Sight => settings.sight_confidence_half_life,
        Sense::Hearing => settings.hearing_confidence_half_life,
        Sense::Damage => settings.damage_confidence_half_life,
        Sense::Touch => settings.touch_confidence_half_life,
        Sense::Custom(id) => senses.and_then(|senses| senses.half_life(id)),
    };
    raw.filter(|half| *half > 0)
}

fn any_half_life(settings: &PerceptionSettings, senses: Option<&PerceptionSenses>) -> bool {
    settings.sight_confidence_half_life.is_some_and(|half| half > 0)
        || settings.hearing_confidence_half_life.is_some_and(|half| half > 0)
        || settings.damage_confidence_half_life.is_some_and(|half| half > 0)
        || settings.touch_confidence_half_life.is_some_and(|half| half > 0)
        || senses.is_some_and(PerceptionSenses::any_half_life)
}

fn decay_stimuli(
    perceived: &mut Perceived,
    tick: u64,
    settings: &PerceptionSettings,
    senses: Option<&PerceptionSenses>,
) {
    for stimulus in &mut perceived.stimuli {
        if stimulus.last_sensed_tick >= tick {
            continue;
        }
        let half = half_life_of(settings, senses, stimulus.sense);
        stimulus.confidence = confidence_after_tick(stimulus.confidence, half);
        stimulus.last_known_position =
            age_position(stimulus.last_known_position, stimulus.last_known_velocity, half);
    }
}

/// How many rays one in-cone pair casts. One when the observer asked for a single sample or the
/// target has no extents. Never more than the budget, so one target cannot stall the sweep, and
/// never more than 64.
fn rays_for(samples: u32, bounds: Vec3, budget: u32) -> u32 {
    let n = if samples <= 1 || !bounds.is_finite() || bounds.length_squared() == 0.0 {
        1
    } else {
        samples.min(64)
    };
    n.min(budget.max(1))
}

fn visibility_of(factor: f32) -> f32 {
    finite_unit(factor)
}

/// The event writers and the spawn-order key [`record`] needs, in one value so the function stays
/// within the argument cap.
struct RecordSink<'a, G, L> {
    gained: G,
    lost: L,
    key: &'a dyn Fn(Entity) -> regia_ecs::spawn_order::SpawnKey,
}

/// Records one sensing in `perceived`, firing [`PerceptionGained`] for a new stimulus and
/// [`PerceptionLost`] for one the capacity pushes out. `sink.key` is the source's [`Perceivable`]
/// spawn-order key (eviction ties).
fn record<G, L>(
    agent: Entity,
    perceived: &mut Perceived,
    sensed: Sensed,
    tick: u64,
    half_life: Option<u32>,
    mut sink: RecordSink<'_, G, L>,
) where
    G: FnMut(PerceptionGained),
    L: FnMut(PerceptionLost),
{
    let sort_key = stimulus_key(sensed.source, sensed.sense);
    match perceived
        .stimuli
        .binary_search_by_key(&sort_key, |s| stimulus_key(s.entity, s.sense))
    {
        Ok(index) => {
            let stimulus = &mut perceived.stimuli[index];
            if !sensed.direct && stimulus.last_sensed_tick >= sensed.reported_tick {
                return;
            }
            if sensed.direct {
                let dt = tick.saturating_sub(stimulus.last_sensed_tick).max(1) as f32;
                // Decay runs after sensing, so a half-life has already stepped the position on
                // each unseen tick before this one. Rewind those steps to the last sensed point.
                // With no half-life the stored position is still that point.
                let previous = if half_life.is_some() {
                    let aged = tick.saturating_sub(stimulus.last_sensed_tick).saturating_sub(1) as f32;
                    let rewound = stimulus.last_known_position - stimulus.last_known_velocity * aged;
                    if rewound.is_finite() {
                        rewound
                    } else {
                        stimulus.last_known_position
                    }
                } else {
                    stimulus.last_known_position
                };
                let step = (sensed.position - previous) / dt;
                if step.is_finite() {
                    stimulus.last_known_velocity = step;
                }
                stimulus.confidence = 1.0;
            } else {
                stimulus.confidence = finite_unit(sensed.confidence);
                stimulus.last_known_velocity = finite_velocity(sensed.velocity);
            }
            stimulus.team = sensed.team;
            if sensed.position.is_finite() {
                stimulus.last_known_position = sensed.position;
            }
            stimulus.last_sensed_tick = tick;
            stimulus.strength = sensed.strength;
        }
        Err(_) => {
            let capacity = perceived.capacity as usize;
            if capacity == 0 {
                return;
            }
            while perceived.stimuli.len() >= capacity {
                evict_one(agent, perceived, &mut sink.lost, sink.key);
            }
            let at = perceived
                .stimuli
                .partition_point(|s| stimulus_key(s.entity, s.sense) < sort_key);
            let (confidence, velocity) = if sensed.direct {
                (1.0, Vec3::ZERO)
            } else {
                (finite_unit(sensed.confidence), finite_velocity(sensed.velocity))
            };
            perceived.stimuli.insert(
                at,
                Stimulus {
                    entity: sensed.source,
                    sense: sensed.sense,
                    team: sensed.team,
                    last_known_position: sensed.position,
                    last_sensed_tick: tick,
                    strength: sensed.strength,
                    confidence,
                    last_known_velocity: velocity,
                },
            );
            (sink.gained)(PerceptionGained {
                agent,
                source: sensed.source,
                sense: sensed.sense,
                position: sensed.position,
                strength: sensed.strength,
            });
        }
    }
}

/// `1` at the listener or eye, `0` at the edge of `range`. A zero range reads as 1: a sound at the
/// listener's own spot is as loud as it gets.
fn nearness(distance: f32, range: f32) -> f32 {
    if range > 0.0 {
        (1.0 - distance / range).clamp(0.0, 1.0)
    } else {
        1.0
    }
}

/// Picks the current target of an agent of `team` (see [`Perceived::target`]). `key` is the source's
/// [`Perceivable`] spawn-order key: an exact score tie goes to the earlier arrival, never to the
/// lower entity id (ids are record-time allocations, ADR-1347).
fn pick_target(
    perceived: &Perceived,
    team: u8,
    attitudes: &Attitudes,
    tick: u64,
    key: &dyn Fn(Entity) -> regia_ecs::spawn_order::SpawnKey,
) -> Entity {
    let max_age = perceived.max_age_ticks.max(1) as f32;
    let mut best = Entity::PLACEHOLDER;
    let mut best_score = f32::NEG_INFINITY;
    for stimulus in &perceived.stimuli {
        if stimulus.entity == Entity::PLACEHOLDER {
            continue;
        }
        let attitude = stimulus
            .team
            .map_or(Attitude::Hostile, |source| attitudes.get(team, source));
        let eligible = match attitude {
            Attitude::Hostile => true,
            Attitude::Neutral => stimulus.sense == Sense::Damage,
            Attitude::Friendly => false,
        };
        if !eligible {
            continue;
        }
        let age = tick.saturating_sub(stimulus.last_sensed_tick) as f32;
        let confidence = finite_unit(stimulus.confidence);
        let mut score = stimulus.strength * confidence * (1.0 - (age / max_age).min(1.0));
        if stimulus.entity == perceived.target {
            score *= CURRENT_TARGET_STICKINESS;
        }
        if score > best_score || (score == best_score && key(stimulus.entity) < key(best)) {
            best_score = score;
            best = stimulus.entity;
        }
    }
    best
}

/// The newest position known for `entity`, over all its stimuli.
fn target_position(perceived: &Perceived, entity: Entity) -> Option<Vec3> {
    perceived
        .stimuli
        .iter()
        .filter(|s| s.entity == entity)
        .max_by_key(|s| s.last_sensed_tick)
        .map(|s| s.last_known_position)
}

/// Writes `value` to `key` unless it is already there, so a quiet agent's blackboard does not read
/// as changed every step.
fn write_key(blackboard: &mut Mut<Blackboard>, key: &str, value: BbValue) {
    if blackboard.get(key) != Some(&value) {
        blackboard.values.insert(key.to_string(), value);
    }
}

/// `<key><suffix>` in the reusable `buffer`.
fn suffixed<'a>(buffer: &'a mut String, key: &str, suffix: &str) -> &'a str {
    buffer.clear();
    buffer.push_str(key);
    buffer.push_str(suffix);
    buffer
}

/// Writes the current target through to the blackboard (see [`Perceived::target_key`]). Takes no
/// allocation when nothing changed.
fn write_target(
    blackboard: &mut Mut<Blackboard>,
    key: &str,
    target: Entity,
    position: Option<Vec3>,
    buffer: &mut String,
) {
    if key.is_empty() {
        return;
    }
    if let (true, Some(position)) = (target != Entity::PLACEHOLDER, position) {
        write_key(blackboard, key, BbValue::Entity(target.to_bits()));
        write_key(
            blackboard,
            suffixed(buffer, key, "_position"),
            BbValue::Vec3(position),
        );
        write_key(blackboard, suffixed(buffer, key, "_known"), BbValue::Bool(true));
    } else {
        if blackboard.contains(key) {
            blackboard.remove(key);
        }
        let position_key = suffixed(buffer, key, "_position");
        if blackboard.contains(position_key) {
            blackboard.remove(position_key);
        }
        write_key(blackboard, suffixed(buffer, key, "_known"), BbValue::Bool(false));
    }
}

/// Whether the ray from `observer` to `target` runs clear, ignoring both of them and anything
/// parented under them (a held weapon, a child collider), and sensors (a trigger hides nothing).
fn line_clear(
    queries: &WorldQueries<'_>,
    from: Vec3,
    to: Vec3,
    observer: Entity,
    target: Entity,
    poses: &PoseQuery<'_, '_>,
) -> bool {
    let delta = to - from;
    let distance = delta.length();
    let direction = delta.normalize_or_zero();
    if distance <= 0.0 || direction == Vec3::ZERO {
        return true;
    }
    let belongs = |mut entity: Entity| -> bool {
        // The walk ends at the root; the bound only guards a malformed hierarchy.
        for _ in 0..64 {
            if entity == observer || entity == target {
                return true;
            }
            match poses.get(entity) {
                Ok((_, _, Some(link))) => entity = link.parent(),
                _ => return false,
            }
        }
        false
    };
    let predicate = |entity: Entity| !belongs(entity);
    queries
        .raycast(
            from,
            direction,
            distance,
            QueryFilter::new().exclude_sensors().predicate(&predicate),
        )
        .is_none()
}

/// The agents [`perception_tick`] visits.
type AgentQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static Perception,
        &'static mut Perceived,
        Option<&'static mut Blackboard>,
    ),
    Without<LayerParked>,
>;

/// The spawn-order tables perception ticks by: the agents' and the sources' arrival numbers.
#[derive(SystemParam)]
struct SpawnOrders<'w> {
    perception: Option<Res<'w, regia_ecs::spawn_order::SpawnOrder<Perception>>>,
    perceivable: Option<Res<'w, regia_ecs::spawn_order::SpawnOrder<Perceivable>>>,
}

/// Entities that moved since the last perception step: a changed local `Transform`, or a changed
/// `GlobalTransform` (a pose written directly, or propagated from a moved parent).
type MovedQuery<'w, 's> = Query<'w, 's, (), Or<(Changed<Transform>, Changed<GlobalTransform>)>>;

/// The inputs past a system function's 16-parameter cap: orders, scratch, movement, squads,
/// sight samples, visibility, the custom-sense registry and team knowledge.
type PerceptionTail<'w, 's> = (
    SpawnOrders<'w>,
    Local<'s, TickScratch>,
    MovedQuery<'w, 's>,
    Query<'w, 's, &'static AiSquadMember>,
    Query<'w, 's, &'static SightSamples>,
    Query<'w, 's, &'static Visibility>,
    Option<Res<'w, PerceptionSenses>>,
    Option<ResMut<'w, TeamKnowledge>>,
);

/// `FixedUpdate`: one step of perception (see the module docs).
#[allow(clippy::too_many_arguments)] // A system's parameters are its data access.
#[tracing::instrument(skip_all, target = "regia_ai", name = "perception_tick")]
fn perception_tick(
    settings: Res<PerceptionSettings>,
    attitudes: Res<Attitudes>,
    mut stats: ResMut<PerceptionStats>,
    mut intake: ResMut<PerceptionIntake>,
    hearing: Res<Messages<HearingEvent>>,
    damage: Res<Messages<DamageEvent>>,
    mut gained: MessageWriter<PerceptionGained>,
    mut lost: MessageWriter<PerceptionLost>,
    physics: Option<Res<PhysicsWorld>>,
    layers: Option<Res<PhysicsLayers>>,
    entities: &Entities,
    mut agents: AgentQuery<'_, '_>,
    targets: Query<(Entity, &Perceivable), Without<LayerParked>>,
    teams: Query<(Option<&Perceivable>, Option<&Perception>)>,
    poses: PoseQuery<'_, '_>,
    // The rest share this slot: a system function stops at 16 parameters.
    (orders, mut scratch, moved, squads, sight_samples, visibility, senses, mut knowledge): PerceptionTail<
        '_,
        '_,
    >,
) {
    stats.tick += 1;
    let tick = stats.tick;
    let scratch = &mut *scratch;
    // The source's arrival order: tie-breaks (target pick, eviction) follow it, never the entity
    // id, which two runs of the same level can allocate differently (ADR-1347).
    let key = |entity: Entity| {
        orders.perceivable.as_deref().map_or_else(
            || (u64::MAX, entity.index_u32(), entity.to_bits()),
            |order| order.key(entity),
        )
    };

    // The visiting order: arrival order (ADR-2396), never archetype or entity-id order.
    scratch.agents.clear();
    scratch.agents.extend(agents.iter().map(|row| row.0));
    crate::agent_order_by(orders.perception.as_deref(), &mut scratch.agents);

    // Hearing and damage: what the frames since the last step queued, then what this frame wrote.
    intake.collect(&hearing, &damage);
    stats.messages_dropped = intake.dropped;
    stats.hearing_tests_last_tick = 0;
    // `moved` only reports changes since the last run, which is every tick: a move on a tick
    // with no sound must still drop the grid, or the next sound is heard at the old positions.
    if !moved.is_empty() {
        scratch.hear_built = false;
    }
    if intake.hearing.iter().any(|event| event.position.is_finite()) {
        // A parent move changes a child's world position without marking the child's
        // `Transform`, so any changed transform drops the grid. The `GlobalTransform` half of
        // `moved` covers a pose that is written directly (an entity with no `Transform`).
        let same_agents = scratch.hear_samples.len() == scratch.agents.len()
            && scratch
                .hear_samples
                .iter()
                .zip(scratch.agents.iter())
                .all(|(sample, entity)| sample.entity == *entity);
        if !(scratch.hear_built && same_agents && moved.is_empty()) {
            scratch.hear_samples.clear();
            scratch.hear_grid.clear();
            for &agent in &scratch.agents {
                let posed = world_pose(agent, &poses);
                let (position, is_posed) = match posed {
                    Some((position, _)) => (position, true),
                    None => (Vec3::ZERO, false),
                };
                let index = scratch.hear_samples.len() as u32;
                scratch.hear_samples.push(HearSample {
                    entity: agent,
                    position,
                    posed: is_posed,
                });
                if is_posed {
                    scratch
                        .hear_grid
                        .entry(hearing_cell(position))
                        .or_default()
                        .push(index);
                }
            }
            scratch.hear_built = true;
        }
    }
    while let Some(event) = intake.hearing.pop_front() {
        stats.hearing_events += 1;
        if !event.position.is_finite() {
            continue;
        }
        collect_hearing_hits(
            &scratch.hear_grid,
            &mut scratch.hear_hits,
            event.position,
            event.radius,
            scratch.hear_samples.len() as u32,
        );
        let mut hit = 0;
        while hit < scratch.hear_hits.len() {
            let sample_index = scratch.hear_hits[hit] as usize;
            hit += 1;
            let (agent, position) = {
                let Some(sample) = scratch.hear_samples.get(sample_index) else {
                    continue;
                };
                if !sample.posed {
                    continue;
                }
                (sample.entity, sample.position)
            };
            if agent == event.source {
                continue;
            }
            let Ok((_, perception, mut perceived, _)) = agents.get_mut(agent) else {
                continue;
            };
            let attitude = attitudes.get(perception.team, event.team);
            if !perceived.detect.allows(attitude) {
                continue;
            }
            stats.hearing_tests_last_tick = stats.hearing_tests_last_tick.saturating_add(1);
            if !can_hear(perception, position, &event) {
                continue;
            }
            let range = perception.hearing_radius.min(event.radius);
            let strength = nearness((event.position - position).length(), range);
            let half = half_life_of(&settings, senses.as_deref(), Sense::Hearing);
            record(
                agent,
                &mut perceived,
                direct_sense(
                    event.source,
                    Sense::Hearing,
                    Some(event.team),
                    event.position,
                    strength,
                ),
                tick,
                half,
                RecordSink {
                    gained: |event| {
                        gained.write(event);
                    },
                    lost: |event| {
                        lost.write(event);
                    },
                    key: &key,
                },
            );
        }
    }
    while let Some(event) = intake.damage.pop_front() {
        stats.damage_events += 1;
        if !event.position.is_finite() || event.attacker == event.victim {
            continue;
        }
        let Ok((_, _, mut perceived, ..)) = agents.get_mut(event.victim) else {
            continue;
        };
        let team = teams
            .get(event.attacker)
            .ok()
            .and_then(|(perceivable, perception)| {
                perceivable.map(|p| p.team).or_else(|| perception.map(|p| p.team))
            });
        let amount = if event.amount.is_finite() {
            event.amount.max(0.0)
        } else {
            0.0
        };
        let full = settings.damage_full_strength;
        let strength = if full.is_finite() && full > 0.0 {
            (amount / full).min(1.0)
        } else {
            1.0
        };
        let half = half_life_of(&settings, senses.as_deref(), Sense::Damage);
        record(
            event.victim,
            &mut perceived,
            direct_sense(event.attacker, Sense::Damage, team, event.position, strength),
            tick,
            half,
            RecordSink {
                gained: |event| {
                    gained.write(event);
                },
                lost: |event| {
                    lost.write(event);
                },
                key: &key,
            },
        );
    }

    // Sight: round-robin over the (agent, target) pairs under the budget.
    let sense_span = tracing::info_span!(target: "regia_ai", "perception_sense");
    let sense_guard = sense_span.entered();
    let sweep = &mut scratch.sweep;
    if !sweep.active && tick >= sweep.next_start {
        sweep.active = true;
        sweep.last = None;
        sweep.next_start = tick.saturating_add(u64::from(settings.sight_interval_ticks.max(1)));
    }
    stats.sight_checks_last_tick = 0;
    stats.sight_rays_last_tick = 0;
    stats.sight_pose_reads_last_tick = 0;
    if sweep.active {
        // Identity only. Poses are read for the pairs this step tests, after the budget check.
        scratch.observers.clear();
        for &agent in &scratch.agents {
            let Ok((entity, perception, perceived, _)) = agents.get(agent) else {
                continue;
            };
            let samples = sight_samples
                .get(entity)
                .map(|samples| u32::from(samples.count))
                .unwrap_or(1);
            scratch.observers.push(Observer {
                entity,
                team: perception.team,
                half_angle: perception.sight_half_angle,
                radius: perception.sight_radius,
                lose_radius: perception.sight_lose_radius,
                lose_half_angle: perception.sight_lose_half_angle,
                detect: perceived.detect,
                samples,
            });
        }
        scratch.candidates.clear();
        for (entity, perceivable) in &targets {
            let bounds = if perceivable.bounds_half_extents.is_finite() {
                perceivable.bounds_half_extents
            } else {
                Vec3::ZERO
            };
            let seen = visibility
                .get(entity)
                .map(|seen| visibility_of(seen.factor))
                .unwrap_or(1.0);
            scratch.candidates.push(Candidate {
                entity,
                team: perceivable.team,
                key: key(entity),
                bounds,
                visibility: seen,
            });
        }
        scratch.candidates.sort_unstable_by_key(|c| c.key);

        let queries = physics.as_deref().map(|world| WorldQueries {
            world,
            layers: layers.as_deref(),
        });
        let (mut observer_index, mut candidate_index) = match sweep.last {
            None => (0, 0),
            Some((last_observer, last_candidate)) => {
                // The observers are in spawn (arrival) order, so the resume point is found by
                // identity. The candidate cursor is the perceivable spawn-order key, valid only
                // on the observer the sweep stopped on; a missing observer (despawned or
                // poseless this tick) resumes at the start, where re-checking a pair only
                // refreshes it.
                match scratch.observers.iter().position(|o| o.entity == last_observer) {
                    Some(index) => {
                        let next = scratch.candidates.partition_point(|c| c.key <= last_candidate);
                        (index, next)
                    }
                    None => (0, 0),
                }
            }
        };
        let budget = settings.sight_checks_per_tick;
        let mut checks = 0u32;
        let mut rays = 0u32;
        // One observer tested against several candidates reads its pose once this step.
        let mut posed_observer: Option<(Entity, Vec3, Vec3)> = None;
        loop {
            let Some(&observer) = scratch.observers.get(observer_index) else {
                sweep.active = false;
                stats.sweeps_completed += 1;
                break;
            };
            let Some(&candidate) = scratch.candidates.get(candidate_index) else {
                observer_index += 1;
                candidate_index = 0;
                posed_observer = None;
                continue;
            };
            if candidate.entity == observer.entity {
                candidate_index += 1;
                continue;
            }
            if checks >= budget {
                break;
            }
            let posed = if let Some((entity, eye, forward)) = posed_observer
                && entity == observer.entity
            {
                Some((eye, forward))
            } else if let Some((position, forward)) = world_pose(observer.entity, &poses) {
                stats.sight_pose_reads_last_tick = stats.sight_pose_reads_last_tick.saturating_add(1);
                let eye_height = agents
                    .get(observer.entity)
                    .ok()
                    .map(|(_, _, perceived, _)| perceived.eye_height)
                    .unwrap_or(1.5);
                let eye = position + Vec3::Y * eye_height;
                posed_observer = Some((observer.entity, eye, forward));
                Some((eye, forward))
            } else {
                None
            };
            let Some((eye, forward)) = posed else {
                // No pose: the pair is not a check, and the sweep moves on.
                candidate_index += 1;
                continue;
            };
            let Some((position, _)) = world_pose(candidate.entity, &poses) else {
                candidate_index += 1;
                continue;
            };
            stats.sight_pose_reads_last_tick = stats.sight_pose_reads_last_tick.saturating_add(1);
            let aim_height = targets
                .get(candidate.entity)
                .ok()
                .map(|(_, perceivable)| perceivable.aim_height)
                .unwrap_or(1.0);
            let aim = position + Vec3::Y * aim_height;
            let allows = observer
                .detect
                .allows(attitudes.get(observer.team, candidate.team));
            let remembered = agents
                .get(observer.entity)
                .is_ok_and(|(_, _, perceived, _)| perceived.get(candidate.entity, Sense::Sight).is_some());
            let in_cone = sight_holds(
                eye,
                forward,
                aim,
                SightRanges {
                    acquire_radius: observer.radius,
                    acquire_half_angle: observer.half_angle,
                    lose_radius: observer.lose_radius,
                    lose_half_angle: observer.lose_half_angle,
                },
                remembered,
            );
            let n_cast = rays_for(observer.samples, candidate.bounds, budget);
            // A pair whose samples would pass the ray budget is left for the next step. The
            // cursor stays on the previous pair, so the resume lands here.
            if allows && in_cone && queries.is_some() && rays.saturating_add(n_cast) > budget {
                break;
            }
            checks += 1;
            sweep.last = Some((observer.entity, candidate.key));
            candidate_index += 1;
            if !allows || !in_cone {
                continue;
            }
            let fraction = if let Some(queries) = &queries {
                let mut clear = 0u32;
                for index in 0..n_cast {
                    let point = aim + sample_offset(index, n_cast, candidate.bounds);
                    rays = rays.saturating_add(1);
                    if line_clear(queries, eye, point, observer.entity, candidate.entity, &poses) {
                        clear = clear.saturating_add(1);
                    }
                }
                if n_cast == 0 {
                    0.0
                } else {
                    clear as f32 / n_cast as f32
                }
            } else {
                1.0
            };
            let threshold = settings.detection_threshold;
            let metering = threshold.is_finite() && threshold > 0.0;
            let strength = nearness((aim - eye).length(), observer.radius);
            let mut record_sight = |perceived: &mut Perceived| {
                record(
                    observer.entity,
                    perceived,
                    direct_sense(
                        candidate.entity,
                        Sense::Sight,
                        Some(candidate.team),
                        position,
                        strength,
                    ),
                    tick,
                    half_life_of(&settings, senses.as_deref(), Sense::Sight),
                    RecordSink {
                        gained: |event| {
                            gained.write(event);
                        },
                        lost: |event| {
                            lost.write(event);
                        },
                        key: &key,
                    },
                );
            };
            if remembered || !metering {
                if fraction > 0.0
                    && let Ok((_, _, mut perceived, ..)) = agents.get_mut(observer.entity)
                {
                    record_sight(&mut perceived);
                }
            } else if fraction <= 0.0 {
                if let Ok((_, _, mut perceived, ..)) = agents.get_mut(observer.entity)
                    && let Some(slot) = perceived
                        .meter
                        .iter_mut()
                        .find(|sample| sample.target == candidate.entity)
                {
                    slot.evidence = (slot.evidence * 0.5).max(0.0);
                }
            } else if let Ok((_, _, mut perceived, ..)) = agents.get_mut(observer.entity) {
                let falloff = cone_falloff(eye, forward, aim, observer.half_angle);
                let gain = detection_gain(fraction, candidate.visibility, falloff);
                let current = perceived
                    .meter
                    .iter()
                    .find(|sample| sample.target == candidate.entity)
                    .map(|sample| sample.evidence)
                    .unwrap_or(0.0);
                let (next, acquired) = meter_acquires(current, gain, threshold);
                if let Some(slot) = perceived
                    .meter
                    .iter_mut()
                    .find(|sample| sample.target == candidate.entity)
                {
                    slot.evidence = next;
                } else if perceived.meter.len() < 64 {
                    perceived.meter.push(DetectionSample {
                        target: candidate.entity,
                        evidence: next,
                    });
                } else if let Some((index, _)) = perceived
                    .meter
                    .iter()
                    .enumerate()
                    .min_by(|left, right| left.1.evidence.total_cmp(&right.1.evidence))
                {
                    perceived.meter[index] = DetectionSample {
                        target: candidate.entity,
                        evidence: next,
                    };
                }
                if acquired {
                    record_sight(&mut perceived);
                }
            }
        }
        stats.sight_checks_last_tick = checks;
        stats.sight_rays_last_tick = rays;
        stats.sight_checks_total += u64::from(checks);
    }

    // Touch: contact inside the radius, no cone and no line of sight. Skipped entirely when every
    // radius is 0, which is the default, so the steady step does not build a second candidate list.
    if tick >= scratch.touch_next
        && scratch.agents.iter().any(|&agent| {
            agents.get(agent).is_ok_and(|(_, perception, ..)| {
                perception.touch_radius.is_finite() && perception.touch_radius > 0.0
            })
        })
    {
        if scratch.candidates.is_empty() || !sweep.active {
            scratch.candidates.clear();
            for (entity, perceivable) in &targets {
                scratch.candidates.push(Candidate {
                    entity,
                    team: perceivable.team,
                    key: key(entity),
                    bounds: Vec3::ZERO,
                    visibility: 1.0,
                });
            }
            scratch.candidates.sort_unstable_by_key(|candidate| candidate.key);
        }
        let agent_count = scratch.agents.len();
        for agent_index in 0..agent_count {
            let agent = scratch.agents[agent_index];
            let Ok((_, perception, perceived, _)) = agents.get(agent) else {
                continue;
            };
            let radius = perception.touch_radius;
            if !(radius.is_finite() && radius > 0.0) {
                continue;
            }
            let detect = perceived.detect;
            let team = perception.team;
            let Some((origin, _)) = world_pose(agent, &poses) else {
                continue;
            };
            let candidate_count = scratch.candidates.len();
            for index in 0..candidate_count {
                let candidate = scratch.candidates[index];
                if candidate.entity == agent || !detect.allows(attitudes.get(team, candidate.team)) {
                    continue;
                }
                let Some((position, _)) = world_pose(candidate.entity, &poses) else {
                    continue;
                };
                let distance = (position - origin).length();
                if !distance.is_finite() || distance > radius {
                    continue;
                }
                let Ok((_, _, mut perceived, ..)) = agents.get_mut(agent) else {
                    continue;
                };
                record(
                    agent,
                    &mut perceived,
                    direct_sense(
                        candidate.entity,
                        Sense::Touch,
                        Some(candidate.team),
                        position,
                        nearness(distance, radius),
                    ),
                    tick,
                    half_life_of(&settings, senses.as_deref(), Sense::Touch),
                    RecordSink {
                        gained: |event| {
                            gained.write(event);
                        },
                        lost: |event| {
                            lost.write(event);
                        },
                        key: &key,
                    },
                );
            }
        }
        scratch.touch_next = tick.saturating_add(u64::from(settings.touch_interval_ticks.max(1)));
    }
    drop(sense_guard);

    let senses_now = senses.as_deref();
    if any_half_life(&settings, senses_now) {
        let agent_count = scratch.agents.len();
        for index in 0..agent_count {
            let agent = scratch.agents[index];
            if let Ok((_, _, mut perceived, _)) = agents.get_mut(agent) {
                decay_stimuli(&mut perceived, tick, &settings, senses_now);
            }
        }
    }

    let any_share = scratch.agents.iter().any(|&agent| {
        agents
            .get(agent)
            .is_ok_and(|(_, perception, ..)| perception.share_knowledge)
    });
    if any_share {
        let ids = scratch.agents.clone();
        if let Some(knowledge) = knowledge.as_deref_mut() {
            knowledge.decay();
            for &agent in &ids {
                let Ok((_, perception, perceived, _)) = agents.get(agent) else {
                    continue;
                };
                if !perception.share_knowledge {
                    continue;
                }
                let team = perception.team;
                let squad = squads.get(agent).ok().map(|member| member.squad.to_bits());
                let facts: Vec<SharedStimulus> = perceived
                    .stimuli
                    .iter()
                    .map(|stimulus| SharedStimulus {
                        entity: stimulus.entity,
                        sense: stimulus.sense,
                        team: stimulus.team,
                        position: stimulus.last_known_position,
                        velocity: stimulus.last_known_velocity,
                        confidence: stimulus.confidence,
                        strength: stimulus.strength,
                        sensed_tick: stimulus.last_sensed_tick,
                        reporter: agent,
                    })
                    .collect();
                for fact in facts {
                    knowledge.upsert_team(team, fact);
                    if let Some(squad) = squad {
                        knowledge.upsert_squad(squad, fact);
                    }
                }
            }
        }
        for &agent in &ids {
            let Ok((_, perception, _, _)) = agents.get(agent) else {
                continue;
            };
            if !perception.share_knowledge {
                continue;
            }
            let team = perception.team;
            let squad = squads.get(agent).ok().map(|member| member.squad);
            scratch.share_in.clear();
            if let Some(knowledge) = knowledge.as_deref() {
                for fact in knowledge.team(team) {
                    if fact.reporter != agent {
                        scratch.share_in.push(*fact);
                    }
                }
                if let Some(squad) = squad {
                    for fact in knowledge.squad(squad) {
                        if fact.reporter != agent {
                            scratch.share_in.push(*fact);
                        }
                    }
                }
            }
            let incoming = scratch.share_in.len();
            for index in 0..incoming {
                let fact = scratch.share_in[index];
                let Ok((_, _, mut perceived, _)) = agents.get_mut(agent) else {
                    continue;
                };
                let half = half_life_of(&settings, senses.as_deref(), fact.sense);
                record(
                    agent,
                    &mut perceived,
                    Sensed {
                        source: fact.entity,
                        sense: fact.sense,
                        team: fact.team,
                        position: fact.position,
                        strength: fact.strength,
                        direct: false,
                        confidence: fact.confidence,
                        velocity: fact.velocity,
                        reported_tick: fact.sensed_tick,
                    },
                    tick,
                    half,
                    RecordSink {
                        gained: |event| {
                            gained.write(event);
                        },
                        lost: |event| {
                            lost.write(event);
                        },
                        key: &key,
                    },
                );
            }
        }
    }

    // Forgetting, the current target and the write-through, for every agent every step: memory ages
    // on the clock, not on the sight budget.
    let agent_count = scratch.agents.len();
    for index in 0..agent_count {
        let agent = scratch.agents[index];
        let Ok((_, perception, mut perceived, blackboard)) = agents.get_mut(agent) else {
            continue;
        };
        let max_age = perceived.max_age_ticks;
        let gone = |s: &Stimulus| s.entity != Entity::PLACEHOLDER && !entities.contains_spawned(s.entity);
        let expired = |s: &Stimulus| tick.saturating_sub(s.last_sensed_tick) > max_age;
        if perceived.stimuli.iter().any(|s| gone(s) || expired(s)) {
            scratch.expired_sights.clear();
            for stimulus in &perceived.stimuli {
                if stimulus.sense == Sense::Sight
                    && stimulus.entity != Entity::PLACEHOLDER
                    && (gone(stimulus) || expired(stimulus))
                {
                    scratch.expired_sights.push(stimulus.entity);
                }
            }
            perceived.stimuli.retain(|s| {
                let reason = if gone(s) {
                    LostReason::Despawned
                } else if expired(s) {
                    LostReason::Expired
                } else {
                    return true;
                };
                lost.write(PerceptionLost {
                    agent,
                    source: s.entity,
                    sense: s.sense,
                    reason,
                });
                false
            });
            if !scratch.expired_sights.is_empty() {
                perceived
                    .meter
                    .retain(|sample| !scratch.expired_sights.contains(&sample.target));
            }
        }
        // A capacity lowered at run time holds from this step, not only when something new arrives.
        while perceived.stimuli.len() > perceived.capacity as usize {
            evict_one(
                agent,
                &mut perceived,
                &mut |event| {
                    lost.write(event);
                },
                &key,
            );
        }
        let target = pick_target(&perceived, perception.team, &attitudes, tick, &key);
        if perceived.target != target {
            perceived.target = target;
        }
        if let Some(mut blackboard) = blackboard {
            let position = target_position(&perceived, target);
            write_target(
                &mut blackboard,
                &perceived.target_key,
                target,
                position,
                &mut scratch.key,
            );
        }
    }
}

/// World pose for the exclusive custom-sense pass. The same walk as [`world_pose`].
fn pose_of(world: &World, entity: Entity) -> Option<(Vec3, Vec3)> {
    crate::tick_budget::pose_from_chain(
        world.get::<Transform>(entity),
        world.get::<GlobalTransform>(entity),
        world.get::<ChildOf>(entity),
        |parent| {
            (
                world.get::<Transform>(parent),
                world.get::<GlobalTransform>(parent),
                world.get::<ChildOf>(parent),
            )
        },
    )
}

struct DueSense {
    id: u16,
    range: f32,
    interval: u32,
    callback: SenseCallback,
}

struct SenseHit {
    observer: Entity,
    sensed: Sensed,
}

/// Custom senses run after [`perception_tick`], so a callback sees this step's world and the
/// stimulus is readable before the frame ends. An empty registry returns before any buffer is
/// built: the steady step stays allocation-free. The span is entered only when a sense is due.
fn custom_senses(world: &mut World) {
    let idle = world
        .get_resource::<PerceptionSenses>()
        .is_none_or(|senses| senses.is_empty());
    if idle {
        return;
    }
    let Some(tick) = world.get_resource::<PerceptionStats>().map(|stats| stats.tick) else {
        return;
    };
    let any_due = world
        .get_resource::<PerceptionSenses>()
        .is_some_and(|senses| senses.entries.iter().any(|entry| tick >= entry.next_tick));
    if !any_due {
        return;
    }
    let _span = tracing::info_span!(target: "regia_ai", "perception_custom_senses").entered();

    let due: Vec<DueSense> = world
        .get_resource::<PerceptionSenses>()
        .map(|senses| {
            senses
                .entries
                .iter()
                .filter(|entry| tick >= entry.next_tick)
                .map(|entry| DueSense {
                    id: entry.id,
                    range: entry.range,
                    interval: entry.interval_ticks,
                    callback: entry.callback,
                })
                .collect()
        })
        .unwrap_or_default();
    if due.is_empty() {
        return;
    }

    let observers: Vec<(Entity, u8, AttitudeFilter)> = {
        let mut query = world.query_filtered::<(Entity, &Perception, &Perceived), Without<LayerParked>>();
        query
            .iter(world)
            .map(|(entity, perception, perceived)| (entity, perception.team, perceived.detect))
            .collect()
    };
    let candidates: Vec<(Entity, u8)> = {
        let mut query = world.query_filtered::<(Entity, &Perceivable), Without<LayerParked>>();
        query
            .iter(world)
            .map(|(entity, perceivable)| (entity, perceivable.team))
            .collect()
    };
    let posed_observers: Vec<(Entity, u8, AttitudeFilter, Vec3)> = observers
        .iter()
        .filter_map(|(entity, team, detect)| {
            let (position, _) = pose_of(world, *entity)?;
            Some((*entity, *team, *detect, position))
        })
        .collect();
    let posed_candidates: Vec<(Entity, u8, Vec3)> = candidates
        .iter()
        .filter_map(|(entity, team)| {
            let (position, _) = pose_of(world, *entity)?;
            Some((*entity, *team, position))
        })
        .collect();

    let mut hits: Vec<SenseHit> = Vec::new();
    for (observer, team, detect, origin) in &posed_observers {
        for (candidate, candidate_team, position) in &posed_candidates {
            if observer == candidate {
                continue;
            }
            let attitude = world
                .get_resource::<Attitudes>()
                .map(|attitudes| attitudes.get(*team, *candidate_team))
                .unwrap_or(Attitude::Hostile);
            if !detect.allows(attitude) {
                continue;
            }
            let distance = (*position - *origin).length();
            for sense in &due {
                if sense.range.is_finite()
                    && sense.range > 0.0
                    && (!distance.is_finite() || distance > sense.range)
                {
                    continue;
                }
                let Some(strength) = (sense.callback)(world, *observer, *candidate) else {
                    continue;
                };
                if !strength.is_finite() {
                    continue;
                }
                hits.push(SenseHit {
                    observer: *observer,
                    sensed: direct_sense(
                        *candidate,
                        Sense::Custom(sense.id),
                        Some(*candidate_team),
                        *position,
                        strength.clamp(0.0, 1.0),
                    ),
                });
            }
        }
    }

    if let Some(mut senses) = world.get_resource_mut::<PerceptionSenses>() {
        for sense in &due {
            if let Some(entry) = senses.entries.iter_mut().find(|entry| entry.id == sense.id) {
                entry.next_tick = tick.saturating_add(u64::from(sense.interval.max(1)));
            }
        }
    }

    let keys: Vec<(Entity, regia_ecs::spawn_order::SpawnKey)> = {
        let entities: Vec<Entity> = {
            let mut query = world.query::<(Entity, &Perceivable)>();
            query.iter(world).map(|(entity, _)| entity).collect()
        };
        let order = world.get_resource::<regia_ecs::spawn_order::SpawnOrder<Perceivable>>();
        entities
            .into_iter()
            .map(|entity| {
                let key = order.map(|order| order.key(entity)).unwrap_or((
                    u64::MAX,
                    entity.index_u32(),
                    entity.to_bits(),
                ));
                (entity, key)
            })
            .collect()
    };
    let key_of = |entity: Entity| {
        keys.iter()
            .find(|(found, _)| *found == entity)
            .map(|(_, key)| *key)
            .unwrap_or((u64::MAX, entity.index_u32(), entity.to_bits()))
    };

    let mut gained_msgs = Vec::new();
    let mut lost_msgs = Vec::new();
    for hit in &hits {
        let half = {
            let settings = world.get_resource::<PerceptionSettings>();
            let custom = world.get_resource::<PerceptionSenses>();
            settings
                .as_ref()
                .and_then(|settings| half_life_of(settings, custom, hit.sensed.sense))
        };
        let Some(mut perceived) = world.get_mut::<Perceived>(hit.observer) else {
            continue;
        };
        record(
            hit.observer,
            &mut perceived,
            hit.sensed,
            tick,
            half,
            RecordSink {
                gained: |event| gained_msgs.push(event),
                lost: |event| lost_msgs.push(event),
                key: &key_of,
            },
        );
    }
    for event in gained_msgs {
        let _ = world.write_message(event);
    }
    for event in lost_msgs {
        let _ = world.write_message(event);
    }
}

/// Memory points, shared facts and detection meters. Runs only while the perception gizmo group
/// is enabled.
#[cfg(feature = "debug_draw")]
fn draw_memory_gizmos(
    agents: Query<(&GlobalTransform, &Perceived), Without<LayerParked>>,
    knowledge: Option<Res<TeamKnowledge>>,
    mut gizmos: Gizmos<AiPerceptionGizmos>,
) {
    for (transform, perceived) in &agents {
        let origin = transform.translation();
        for stimulus in &perceived.stimuli {
            crate::perception::draw_memory_point(
                &mut gizmos,
                stimulus.last_known_position,
                stimulus.confidence,
            );
        }
        for sample in &perceived.meter {
            crate::perception::draw_meter_bar(&mut gizmos, origin, sample.evidence);
        }
    }
    if let Some(knowledge) = knowledge.as_deref() {
        for fact in knowledge.iter_facts() {
            crate::perception::draw_team_marker(&mut gizmos, fact.position);
        }
    }
}
