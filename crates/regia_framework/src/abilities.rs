// SPDX-License-Identifier: MPL-2.0
//! Abilities, attributes and effects (`docs/specs/53-abilities-attributes-effects.md`): one
//! data-driven, fixed-step, deterministic model of numbers on entities, timed changes to them and
//! triggerable actions; guide: `book/agents/abilities.md`.
//!
//! - [`Attributes`]: named `f32` values with a durable `base` and a derived `current`, clamps,
//!   `max_from` links and derived values (§ 2.2).
//! - Modifiers ([`ModOp`], [`Magnitude`]): five phases evaluated from scratch every step in a
//!   fixed float order (§ 2.3); [`explain_attribute`] shows the breakdown.
//! - Effects ([`EffectAsset`], [`AppliedEffects`], [`apply_effect`]): instant, timed or until
//!   removed, with pulses, stacking, suspension, immunity, cleanses and chains (§ 2.4, § 2.5).
//! - Abilities ([`AbilityAsset`], [`AbilityBook`], [`activate`]): grants with a small state machine,
//!   tag requirements, cost, cooldown, cancel and block rules, triggers, input and picks (§ 2.6).
//! - Cues ([`CueEvent`]): presentation-only messages (§ 2.7).
//!
//! Everything runs in `FixedUpdate` in the chained [`AbilitySet`] sets. Assets are read from the
//! [`AbilityAssets`] resource by path (`.attrs.ron`, `.effect.ron`, `.ability.ron`).
//!
//! ```
//! use regia_framework::abilities::{AttrId, Attribute, Attributes};
//!
//! let attrs = Attributes::from_list([
//!     Attribute::new("health_max", 100.0, 1.0, 9999.0),
//!     Attribute::new("health", 100.0, 0.0, 9999.0).with_max_from("health_max"),
//! ])
//! .expect("valid attributes");
//! assert_eq!(attrs.get(AttrId::of("health")), Some(100.0));
//! ```

mod assets;
mod attributes;
mod bridge;
mod cooked;
mod cues;
mod effects;
mod grants;
mod hash;
pub mod migrate;
mod modifiers;
mod stacking;
// Cook order and the runtime loader by asset path (ADR-9370).
mod load;

pub use assets::{
    ABILITY_FORMAT_VERSION, ATTRIBUTES_FORMAT_VERSION, AbilityAssets, AssetKind, AttributeDecl,
    AttributesAsset, DerivedDecl, Diagnostic, EFFECT_FORMAT_VERSION, MAX_ATTRIBUTES_PER_ENTITY, Severity,
    ability_clone_count, ability_migration_sources, ability_schema_fingerprint, attributes_migration_sources,
    attributes_schema_fingerprint, effect_migration_sources, effect_schema_fingerprint, has_errors,
    parse_ron, to_ron, validate_ability, validate_attributes, validate_effect,
};
pub use attributes::{
    AttrId, AttrLane, Attribute, AttributeChanged, Attributes, Derived, valid_attribute_name,
};
pub use bridge::{HealthBridge, HealthBridgeState, HealthFromAttributes};
pub use cooked::{
    ABILITIES_COOKED_VERSION, CookedAbilityAsset, abilities_cooked_schema_fingerprint, cooked_bytes,
    from_cooked_bytes,
};
pub use cues::{CUE_RING, CueAt, CueBinding, CueEvent, CueKind, CueLog, CueParams};
pub use effects::{
    AppliedEffect, AppliedEffects, ApplyEffect, ApplyOutcome, Chain, EffectApplied, EffectAsset, EffectEnded,
    EffectGrant, EffectRefused, EffectiveTagsChanged, EndReason, Lifetime, MAX_CHAIN_DEPTH,
    MAX_EFFECTS_PER_ENTITY, MAX_EFFECTS_TOTAL, MAX_MODIFIERS_PER_EFFECT, MAX_PULSES_PER_STEP, OnEffectEnd,
    OnResume, Outcome, Pulse, SOFT_EFFECTS_PER_ENTITY, StacksChanged, SuspendRule, TagCounts, apply_effect,
    chance_replay_steps, effective_container, effective_has, effective_matches, explain_attribute,
    remove_effect, remove_effects_matching, set_suspended, ticks_from_seconds,
};
pub use grants::{
    AbilitiesLocked, AbilityAsset, AbilityBook, AbilityInput, AbilityLifecycle, AbilitySignal, AbilityState,
    CancelAbility, ChargeMode, Grant, GrantOptions, LifecycleEvent, MAX_GRANTS_PER_BOOK,
    MAX_MESSAGES_PER_STEP, MAX_PICKS, Pick, Picks, Refusal, RefusalCause, RevokeAbility, RunInfo, RunSite,
    RunStateMode, StartAbility, StartContext, Trigger, WhenRunning, ability_state, abort_picks, activate,
    activation_check_count, bind_input, block_input, cancel, charge, check_activation, cooldown_tags, end,
    grant_ability, revoke_ability, run_value, send_signal, set_run_value, set_scratch, signal_now,
    submit_picks,
};
/// File suffixes of the three authored ability formats (lowercase). Packages ship them cooked
/// only, never raw (ADR-9850 (c)).
pub const ABILITY_SOURCE_SUFFIXES: [&str; 3] = [".attrs.ron", ".effect.ron", ".ability.ron"];

pub use hash::{ATTRIBUTE_QUANTUM, abilities_state_hash, abilities_state_hash_keyed, quantize};
pub use load::{
    ABILITIES_COOKED_TYPE_PATH, AbilityAssetRegistrations, AbilityAssetsAppExt, AbilityLoad,
    cook_ability_set, is_ability_asset_name, load_ability_assets, load_order, load_order_references,
    load_registered_ability_assets,
};
pub use modifiers::{
    AttributeBreakdown, Contribution, EditGateFn, EditGates, EditRequest, EditVerdict, InlineCurve,
    InlineCurveTables, LevelCurveRef, LevelCurves, LevelCurvesRes, LiveMod, Magnitude, MagnitudeFn,
    MagnitudeFns, MagnitudeInput, ModOp, Modifier, PhaseBreakdown, RunStateDefaultFn, RunStateTypes,
    StatRead, StatSide, evaluate, sample_points,
};
pub use stacking::{OnAdd, OnLapse, PulseOnAdd, StackKey, StackRule, TimeOnAdd, WhenFull};

use std::collections::BTreeMap;

use regia_ecs::ecs::query::QueryFilter;

use regia_app::prelude::*;
use regia_scene::components::EntityGuid;
use serde::Serialize;

use crate::save_game::{RegisterSaveableExt, SaveableResource};
use crate::tags::TagPlugin;

/// Id of a live effect: monotonic per world (from [`AbilityCounters`]), never reused.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct EffectId(pub u64);

/// Id of a grant in an [`AbilityBook`], never reused while the book lives.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct GrantId(pub u32);

/// Reserved client-intent sequence number (0 = none); carried, never interpreted (§ 2.8).
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct IntentSeq(pub u32);

/// Errors of the module; [`reason`](Self::reason) is the agent `data.reason` (§ 5.2).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AbilityError {
    /// No such entity.
    #[error("entity not found")]
    EntityNotFound,
    /// The book already holds [`MAX_GRANTS_PER_BOOK`] grants.
    #[error("the ability book is full ({MAX_GRANTS_PER_BOOK} grants)")]
    GrantLimit,
    /// The entity has no `Attributes`.
    #[error("the entity has no attributes")]
    NoAttributes,
    /// The entity has no attribute with this id.
    #[error("unknown attribute {0:#x}")]
    UnknownAttribute(u64),
    /// Instant edits to a derived attribute are refused.
    #[error("attribute `{0}` is derived")]
    AttributeDerived(String),
    /// A non-finite value was refused.
    #[error("attribute `{0}`: value is not finite")]
    AttributeNotFinite(String),
    /// A name appears twice.
    #[error("attribute `{0}` is declared twice")]
    AttributeDuplicate(String),
    /// `min > max`.
    #[error("attribute `{0}`: min is above max")]
    AttributeRange(String),
    /// A `max_from` or derived cycle.
    #[error("attribute `{0}` is on a cycle")]
    AttributeCycle(String),
    /// A name breaks `[a-z][a-z0-9_]{{0,47}}`.
    #[error("`{0}` is not a valid attribute name")]
    BadName(String),
    /// The effect asset is not loaded.
    #[error("effect `{0}` is not loaded")]
    EffectNotFound(String),
    /// No live effect with this id.
    #[error("no live effect {0}")]
    EffectIdNotFound(u64),
    /// No grant with this id.
    #[error("no grant {0}")]
    GrantNotFound(u32),
    /// The asset is not loaded or the file does not exist.
    #[error("asset `{0}` not found")]
    AssetNotFound(String),
    /// The ability cannot be cancelled.
    #[error("the ability is not cancelable")]
    NotCancelable,
    /// A tag query did not parse.
    #[error("bad query: {0}")]
    BadQuery(String),
    /// A file is newer than this engine reads.
    #[error("format too new")]
    FormatTooNew,
    /// A referenced asset is missing.
    #[error("reference `{0}` is missing")]
    ReferenceMissing(String),
    /// `try_now` failed the activation check.
    #[error("start refused: {}", .0.code())]
    StartRefused(RefusalCause),
    /// RON could not be written or read.
    #[error("format error: {0}")]
    Format(String),
}

impl AbilityError {
    /// The lowercase reason code.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            AbilityError::EntityNotFound => "entity_not_found",
            AbilityError::NoAttributes => "no_attributes",
            AbilityError::UnknownAttribute(_) => "unknown_attribute",
            AbilityError::AttributeDerived(_) => "attribute_derived",
            AbilityError::AttributeNotFinite(_) => "attribute_not_finite",
            AbilityError::AttributeDuplicate(_) => "attribute_duplicate",
            AbilityError::AttributeRange(_) => "attribute_range",
            AbilityError::AttributeCycle(_) => "attribute_cycle",
            AbilityError::BadName(_) => "attribute_name",
            AbilityError::EffectNotFound(_) | AbilityError::EffectIdNotFound(_) => "effect_not_found",
            AbilityError::GrantNotFound(_) => "grant_not_found",
            AbilityError::GrantLimit => "grant_limit",
            AbilityError::AssetNotFound(_) => "asset_not_found",
            AbilityError::NotCancelable => "not_cancelable",
            AbilityError::BadQuery(_) => "bad_query",
            AbilityError::FormatTooNew => "format_too_new",
            AbilityError::ReferenceMissing(_) => "reference_missing",
            AbilityError::StartRefused(cause) => cause.code(),
            AbilityError::Format(_) => "parse_error",
        }
    }
}

/// Dev-tool switches that skip checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cheats {
    /// Skip the cost check and payment.
    pub free_costs: bool,
    /// Skip the cooldown check and payment.
    pub no_cooldowns: bool,
}

/// Suppresses cues per entity at write time.
pub type CueFilter = fn(Entity, &World) -> bool;

/// Module settings.
#[derive(Resource, Debug, Clone)]
pub struct AbilitySettings {
    /// Seed of the chance streams.
    pub seed: u64,
    /// `false` silences every cue.
    pub cues_enabled: bool,
    /// Per-entity cue filter.
    pub cue_filter: Option<CueFilter>,
    /// Dev switches.
    pub cheats: Cheats,
}

impl Default for AbilitySettings {
    fn default() -> Self {
        Self {
            seed: 0x5eed_ab11,
            cues_enabled: true,
            cue_filter: None,
            cheats: Cheats::default(),
        }
    }
}

/// Saved counters: the step counter, the last effect id and the chance stream positions.
#[derive(Resource, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Resource, Default)]
pub struct AbilityCounters {
    /// Fixed steps run by `AbilitySet`.
    pub tick: u64,
    /// The last effect id handed out.
    pub last_effect_id: u64,
    /// `(target key, asset id, position)`, sorted.
    pub rolls: Vec<(u64, u64, u64)>,
}

impl SaveableResource for AbilityCounters {}

impl AbilityCounters {
    pub(crate) fn next_effect_id(&mut self) -> EffectId {
        self.last_effect_id += 1;
        EffectId(self.last_effect_id)
    }

    /// The stream position for `(key, asset)`, advanced once.
    pub(crate) fn next_roll(&mut self, key: u64, asset: u64) -> u64 {
        match self.rolls.binary_search_by(|r| (r.0, r.1).cmp(&(key, asset))) {
            Ok(i) => {
                let p = self.rolls[i].2;
                self.rolls[i].2 += 1;
                p
            }
            Err(i) => {
                self.rolls.insert(i, (key, asset, 1));
                0
            }
        }
    }
}

/// Counters for the agent and budget tests (no wall time).
#[derive(Resource, Debug, Clone, Default, Serialize)]
pub struct AbilityStats {
    /// Entities with `Attributes` at the last recompute.
    pub entities: u64,
    /// Live effects: exact at the last recompute, and counted up and down by applies and ends in
    /// between, so the world cap (`MAX_EFFECTS_TOTAL`) sees this step's effects.
    pub live_effects: u64,
    /// Successful applications (applied or stacked), total.
    pub applications: u64,
    /// Effects ended, total.
    pub removals: u64,
    /// Modifier evaluations in the last recompute.
    pub modifier_evaluations: u64,
    /// Entities skipped by the dirty flag in the last recompute.
    pub entities_skipped: u64,
    /// Signals that matched no trigger, total.
    pub signals_unmatched: u64,
    /// Warning counts by code (`curve_missing`, `param_missing`, `pulse_overrun`, ...).
    pub warnings: BTreeMap<&'static str, u64>,
    /// Effects ended, total, by [`EndReason::code`] (`expired`, `removed`, `cleansed`, `restarted`,
    /// `source_gone`, `target_gone`).
    pub ended: BTreeMap<&'static str, u64>,
}

impl AbilityStats {
    /// How many effects ended with `reason`.
    #[must_use]
    pub fn ended_count(&self, reason: EndReason) -> u64 {
        self.ended.get(reason.code()).copied().unwrap_or(0)
    }

    /// Counts one ending with `reason` (also in `removals`).
    pub(crate) fn count_end(&mut self, reason: EndReason) {
        self.removals += 1;
        self.live_effects = self.live_effects.saturating_sub(1);
        *self.ended.entry(reason.code()).or_insert(0) += 1;
    }

    /// How often warning `code` was logged.
    #[must_use]
    pub fn warning_count(&self, code: &str) -> u64 {
        self.warnings.get(code).copied().unwrap_or(0)
    }
}

/// The chained `FixedUpdate` sets (§ 3.4).
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AbilitySet {
    /// Input messages.
    Input,
    /// Apply/start/cancel/revoke requests: each kind in message order, the kinds in the fixed
    /// order apply -> start -> cancel -> revoke (ADR-7573).
    Requests,
    /// Signals and tag triggers.
    Triggers,
    /// Suspension, pulses, expiry, lapses, chains.
    Effects,
    /// Derived values, modifiers, clamps.
    Recompute,
    /// Ending -> Ready, run limits.
    States,
    /// Change messages.
    Notify,
}

/// Registers one run-state type on a [`RunStateTypes`] under a name.
type RunTypeRegistration = fn(&mut RunStateTypes, &str);

/// Registers the module (§ 3.3). Builder methods register edit gates, magnitude functions and
/// run-state types for the engine owner; a hot-reloadable game registers on the resources with its
/// own owner instead.
#[derive(Default)]
pub struct AbilityPlugin {
    gates: Vec<EditGateFn>,
    fns: Vec<(String, MagnitudeFn)>,
    run_types: Vec<(String, RunTypeRegistration)>,
}

impl AbilityPlugin {
    /// Adds an edit gate.
    #[must_use]
    pub fn gate(mut self, gate: EditGateFn) -> Self {
        self.gates.push(gate);
        self
    }

    /// Adds a magnitude function.
    #[must_use]
    pub fn magnitude_fn(mut self, name: &str, f: MagnitudeFn) -> Self {
        self.fns.push((name.to_owned(), f));
        self
    }

    /// Adds a run-state type.
    #[must_use]
    pub fn run_state<T: Serialize + Default + 'static>(mut self, name: &str) -> Self {
        fn reg<T: Serialize + Default>(types: &mut RunStateTypes, name: &str) {
            types.register::<T>(TypeOwner::Engine, name);
        }
        self.run_types.push((name.to_owned(), reg::<T>));
        self
    }
}

impl Plugin for AbilityPlugin {
    fn build(&self, app: &mut App) {
        if !app.has_plugin(std::any::type_name::<TagPlugin>()) {
            app.add_plugins(TagPlugin);
        }
        let mut gates = EditGates::default();
        for g in &self.gates {
            gates.register(TypeOwner::Engine, *g);
        }
        let mut fns = MagnitudeFns::default();
        for (name, f) in &self.fns {
            fns.register(TypeOwner::Engine, name, *f);
        }
        let mut types = RunStateTypes::default();
        for (name, reg) in &self.run_types {
            reg(&mut types, name);
        }
        app.register_type::<Attributes>()
            .register_type::<AppliedEffects>()
            .register_type::<AbilityBook>()
            .register_type::<TagCounts>()
            .register_type::<AbilitiesLocked>()
            .register_type::<HealthFromAttributes>()
            .register_saveable_resource::<AbilityCounters>()
            .init_resource::<AbilityCounters>()
            .init_resource::<effects::RollStreams>()
            .insert_resource(gates)
            .insert_resource(fns)
            .insert_resource(types)
            .init_resource::<AbilitySettings>()
            .init_resource::<AbilityStats>()
            .init_resource::<AbilityAssets>()
            .init_resource::<LevelCurvesRes>()
            .init_resource::<CueLog>()
            .init_resource::<effects::PendingChains>()
            .add_message::<ApplyEffect>()
            .add_message::<StartAbility>()
            .add_message::<CancelAbility>()
            .add_message::<RevokeAbility>()
            .add_message::<AbilitySignal>()
            .add_message::<AbilityInput>()
            .add_message::<AbilityLifecycle>()
            .add_message::<AttributeChanged>()
            .add_message::<EffectApplied>()
            .add_message::<EffectEnded>()
            .add_message::<EffectRefused>()
            .add_message::<StacksChanged>()
            .add_message::<EffectiveTagsChanged>()
            .add_message::<CueEvent>()
            .add_message::<crate::damage::DamageEvent>()
            .add_message::<crate::damage::DiedEvent>()
            .configure_sets(
                FixedUpdate,
                (
                    AbilitySet::Input,
                    AbilitySet::Requests,
                    AbilitySet::Triggers,
                    AbilitySet::Effects,
                    AbilitySet::Recompute,
                    AbilitySet::States,
                    AbilitySet::Notify,
                )
                    .chain()
                    .before(crate::script::ScriptsSet::FixedUpdate)
                    .before(crate::damage::DamageSet),
            )
            .add_systems(
                FixedUpdate,
                (
                    (advance_tick, grants::input_pass)
                        .chain()
                        .in_set(AbilitySet::Input),
                    grants::requests_pass.in_set(AbilitySet::Requests),
                    grants::triggers_pass.in_set(AbilitySet::Triggers),
                    effects::effects_pass.in_set(AbilitySet::Effects),
                    effects::recompute_pass.in_set(AbilitySet::Recompute),
                    grants::states_pass.in_set(AbilitySet::States),
                    effects::notify_pass.in_set(AbilitySet::Notify),
                ),
            );
    }
}

fn advance_tick(mut counters: ResMut<AbilityCounters>) {
    counters.tick += 1;
}

/// What the § 7 entity order reads; a pass holds a `QueryState` of it filtered by its component.
pub(crate) type OrderItem = (Entity, Option<&'static EntityGuid>);

/// One entity's place in the § 7 order: ascending `EntityGuid`, then those without one by bits.
pub(crate) type OrderKey = (bool, u128, u64);

/// The § 7 sort key of `e` with guid `guid`. Unique per entity (its bits are in it).
pub(crate) fn order_key(e: Entity, guid: Option<&EntityGuid>) -> OrderKey {
    (guid.is_none(), guid.map_or(0, |g| g.0.0), e.to_bits())
}

/// The § 7 entity order of one pass, kept between steps (§ 9: no per-step allocation). It holds
/// the sorted entities alone, one word each (§ 10 memory budget, ADR-2426): a step reuses them
/// when they are still exactly the query's entities and still in ascending key order. Keys are
/// unique, so such a list is the one order a sort would give; else the list is rebuilt and sorted.
#[derive(Default)]
pub(crate) struct EntityOrder {
    entities: Vec<Entity>,
}

impl EntityOrder {
    /// The ordered entities. The caller iterates them while it mutates the world and hands the
    /// list back with [`give_back`](Self::give_back).
    pub(crate) fn take<F: QueryFilter>(
        &mut self,
        world: &World,
        query: &mut QueryState<OrderItem, F>,
    ) -> Vec<Entity> {
        if !self.current(world, query) {
            self.entities.clear();
            self.entities.extend(query.iter(world).map(|(e, _)| e));
            self.entities
                .sort_unstable_by_key(|&e| order_key(e, world.get::<EntityGuid>(e)));
        }
        std::mem::take(&mut self.entities)
    }

    /// Whether the kept list holds exactly the query's entities in ascending key order: as many
    /// as the query matches, each matched, keys strictly rising (so no entity twice).
    fn current<F: QueryFilter>(&self, world: &World, query: &mut QueryState<OrderItem, F>) -> bool {
        // The count brings the query's archetype cache up to date for `get_manual`.
        if query.iter(world).count() != self.entities.len() {
            return false;
        }
        let mut last: Option<OrderKey> = None;
        for &e in &self.entities {
            let Ok((_, guid)) = query.get_manual(world, e) else {
                return false;
            };
            let key = order_key(e, guid);
            if last.is_some_and(|l| l >= key) {
                return false;
            }
            last = Some(key);
        }
        true
    }

    /// Returns the list [`take`](Self::take) handed out.
    pub(crate) fn give_back(&mut self, entities: Vec<Entity>) {
        self.entities = entities;
    }
}

/// The chance-stream key of an entity: the low 64 bits of its `EntityGuid`, or `None` without one
/// (a target without a guid refuses chance effects deterministically instead of drawing from a
/// spawn-order-dependent key; audit G finding 5, ADR-4756).
pub(crate) fn target_key(world: &World, e: Entity) -> Option<u64> {
    world.get::<EntityGuid>(e).map(|g| g.0.0 as u64)
}

/// Every entity with abilities state in the § 7 order, each once.
pub(crate) fn sorted_abilities_entities(world: &mut World) -> Vec<Entity> {
    let mut query = world.query_filtered::<OrderItem, Or<(
        With<Attributes>,
        With<AppliedEffects>,
        With<AbilityBook>,
        With<TagCounts>,
    )>>();
    let mut keyed: Vec<(OrderKey, Entity)> = query.iter(world).map(|(e, g)| (order_key(e, g), e)).collect();
    keyed.sort_unstable_by_key(|k| k.0);
    keyed.into_iter().map(|k| k.1).collect()
}

/// Spawns-or-replaces `Attributes` on `e` from the attribute list at `path`.
pub fn add_attributes(world: &mut World, e: Entity, path: &str) -> Result<usize, AbilityError> {
    let asset = world
        .get_resource::<AbilityAssets>()
        .and_then(|a| a.attributes(path))
        .ok_or_else(|| AbilityError::AssetNotFound(path.to_owned()))?;
    let attrs = asset.build()?;
    let n = attrs.len();
    world
        .get_entity_mut(e)
        .map_err(|_| AbilityError::EntityNotFound)?
        .insert(attrs);
    Ok(n)
}

/// Sets an attribute's base directly (no gates; the agent's `abilities.set_base`). Returns
/// `(old, new)`.
pub fn set_base(
    world: &mut World,
    e: Entity,
    attribute: AttrId,
    value: f32,
) -> Result<(f32, f32), AbilityError> {
    if world.get_entity(e).is_err() {
        return Err(AbilityError::EntityNotFound);
    }
    let mut attrs = world.get_mut::<Attributes>(e).ok_or(AbilityError::NoAttributes)?;
    attrs.set_base(attribute, value)
}

/// Drops every edit gate, magnitude function and run-state type `owner` registered (hot reload
/// teardown, § 3.3).
pub fn drop_owner_registrations(world: &mut World, owner: TypeOwner) {
    if let Some(mut g) = world.get_resource_mut::<EditGates>() {
        g.drop_owner(owner);
    }
    if let Some(mut f) = world.get_resource_mut::<MagnitudeFns>() {
        f.drop_owner(owner);
    }
    if let Some(mut t) = world.get_resource_mut::<RunStateTypes>() {
        t.drop_owner(owner);
    }
}
