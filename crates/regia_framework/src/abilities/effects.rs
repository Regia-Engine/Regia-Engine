// SPDX-License-Identifier: MPL-2.0
//! Effects (`docs/specs/53-abilities-attributes-effects.md` § 2.4): authored packages of
//! modifiers, tags and rules; application, the per-step effect pass, the recompute pass, counted
//! tags and the notify pass.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_ecs::ecs::lifecycle::HookContext;
use regia_ecs::ecs::world::DeferredWorld;
use regia_scene::components::EntityGuid;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use super::attributes::ReportRow;
use super::cues::{self, CueAt, CueBase, CueBinding, CueKind};
use super::grants::{self, GrantOptions};
use super::modifiers::{
    AttributeBreakdown, EditGates, InlineCurve, LevelCurves, LevelCurvesRes, LiveMod, Magnitude,
    MagnitudeFns, MagnitudeInput, Modifier, StatSide, evaluate,
};
use super::stacking::{self, Lapse, Merge, StackRule};
use super::{
    AbilityCounters, AbilityError, AbilitySettings, AbilityStats, AttrId, AttrLane, AttributeChanged,
    Attributes, EffectId, EntityOrder, GrantId, IntentSeq, OrderItem, assets,
};
use crate::tags::{Tag, TagContainer, TagLookup, TagQuery};

/// Live effects per entity above which each further application logs `effects_over_soft_limit`
/// (§ 10, the soft budget; [`MAX_EFFECTS_PER_ENTITY`] is the refusal).
pub const SOFT_EFFECTS_PER_ENTITY: usize = 32;
/// Modifiers per effect, `base_edits` and `live_mods` together, above which `abilities.validate`
/// warns `effect_over_budget` (§ 10). A warning: a file that loaded keeps loading.
pub const MAX_MODIFIERS_PER_EFFECT: usize = 16;
/// Live effects per entity before an application is refused (`limit_reached`, § 10).
pub const MAX_EFFECTS_PER_ENTITY: usize = 128;
/// Live effects in the world (as of the last recompute) before an application is refused
/// (`limit_reached`, § 10).
pub const MAX_EFFECTS_TOTAL: u64 = 32_000;
/// Deepest chain of chained effects (§ 2.4 step 7).
pub const MAX_CHAIN_DEPTH: u8 = 4;
/// Most pulses one effect runs in one step before `pulse_overrun` (§ 8 test 5).
pub const MAX_PULSES_PER_STEP: u32 = 8;

/// How long an effect lives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Lifetime {
    /// Applied once to `base`; never enters [`AppliedEffects`].
    Instant,
    /// Lives for this many seconds. Application converts them with [`ticks_from_seconds`]
    /// (`0.1` s at 60 Hz is 6 ticks).
    Timed(Magnitude),
    /// Lives until removed.
    UntilRemoved,
}

/// A pulse: `base_edits` run every `every` seconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pulse {
    /// Period in seconds (> 0, at least one tick).
    pub every: f32,
    /// Also pulse at application.
    #[serde(default)]
    pub on_apply: bool,
}

/// Effects applied after this one (asset paths).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Chain {
    /// After a successful application.
    #[serde(default)]
    pub on_apply: Vec<String>,
    /// After natural expiry.
    #[serde(default)]
    pub on_lapse: Vec<String>,
    /// After any other removal.
    #[serde(default)]
    pub on_removed: Vec<String>,
}

/// What happens to an ability an effect granted when the effect ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum OnEffectEnd {
    /// Revoke at once (cancelling a run).
    #[default]
    Cancel,
    /// Revoke when the current run ends.
    AfterRun,
    /// Keep the grant.
    Keep,
}

fn one_u32() -> u32 {
    1
}

/// An ability granted while the effect lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGrant {
    /// Ability asset path.
    pub ability: String,
    /// Grant level.
    #[serde(default = "one_u32")]
    pub level: u32,
    /// What the effect's end does to the grant.
    #[serde(default)]
    pub on_effect_end: OnEffectEnd,
}

/// What lifting a suspension does to the pulse timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum OnResume {
    /// Keep the stored next pulse tick.
    #[default]
    Continue,
    /// Next pulse one full period from now.
    Restart,
    /// Pulse now, then a full period.
    FireAndRestart,
}

/// Suspension behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SuspendRule {
    /// Stop the clock while suspended.
    #[serde(default)]
    pub freeze_clock: bool,
    /// Pulse timer on resume.
    #[serde(default)]
    pub on_resume: OnResume,
}

fn one_f32() -> f32 {
    1.0
}

/// An authored effect (`.effect.ron`, § 4.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectAsset {
    /// Format version (1).
    pub format_version: u32,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// What it is for.
    #[serde(default)]
    pub description: String,
    /// How long it lives.
    pub lifetime: Lifetime,
    /// Optional pulse.
    #[serde(default)]
    pub pulse: Option<Pulse>,
    /// Edits to `base` (the only list an instant effect may use; run on every pulse).
    #[serde(default)]
    pub base_edits: Vec<Modifier>,
    /// Modifiers that shape `current` while the effect lives.
    #[serde(default)]
    pub live_mods: Vec<Modifier>,
    /// Tags counted on the target while the effect lives and is not suspended.
    #[serde(default)]
    pub status_tags: Vec<Tag>,
    /// Tags describing the effect itself.
    #[serde(default)]
    pub kind_tags: Vec<Tag>,
    /// The target must match to apply.
    #[serde(default)]
    pub apply_if: Option<TagQuery>,
    /// While the target does not match, the effect is suspended.
    #[serde(default)]
    pub active_if: Option<TagQuery>,
    /// Chance per application, 0..=1.
    #[serde(default = "one_f32")]
    pub chance: f32,
    /// Applications whose `kind_tags` match any query are refused while this lives.
    #[serde(default)]
    pub immunity: Vec<TagQuery>,
    /// Live effects whose `kind_tags` match are removed on success.
    #[serde(default)]
    pub cleanses: Vec<TagQuery>,
    /// Abilities granted while it lives.
    #[serde(default)]
    pub grants: Vec<EffectGrant>,
    /// Effects applied afterwards.
    #[serde(default)]
    pub chain: Chain,
    /// Stacking rule; `None` = every application is its own entry.
    #[serde(default)]
    pub stack: Option<StackRule>,
    /// Suspension behavior.
    #[serde(default)]
    pub suspend: SuspendRule,
    /// End when the source despawns.
    #[serde(default)]
    pub end_with_source: bool,
    /// Cue bindings.
    #[serde(default)]
    pub cues: Vec<CueBinding>,
    /// Inline curves (`Curve((table: "", name))`).
    #[serde(default)]
    pub curves: Vec<InlineCurve>,
}

impl EffectAsset {
    /// A minimal effect with this lifetime.
    #[must_use]
    pub fn new(lifetime: Lifetime) -> Self {
        Self {
            format_version: 1,
            name: String::new(),
            description: String::new(),
            lifetime,
            pulse: None,
            base_edits: Vec::new(),
            live_mods: Vec::new(),
            status_tags: Vec::new(),
            kind_tags: Vec::new(),
            apply_if: None,
            active_if: None,
            chance: 1.0,
            immunity: Vec::new(),
            cleanses: Vec::new(),
            grants: Vec::new(),
            chain: Chain::default(),
            stack: None,
            suspend: SuspendRule::default(),
            end_with_source: false,
            cues: Vec::new(),
            curves: Vec::new(),
        }
    }

    /// Whether the lifetime is `Instant`.
    #[must_use]
    pub fn is_instant(&self) -> bool {
        matches!(self.lifetime, Lifetime::Instant)
    }

    /// The kind tags as a container.
    #[must_use]
    pub fn kind_container(&self) -> TagContainer {
        self.kind_tags.iter().copied().collect()
    }
}

/// One live application.
#[derive(Reflect, Debug, Clone, PartialEq)]
pub struct AppliedEffect {
    /// Id (monotonic per world, never reused).
    pub id: EffectId,
    /// Effect asset path.
    pub asset: String,
    /// Level.
    pub level: u32,
    /// Who applied it (not saved: entity ids are not remapped by the save path, § 4.6).
    #[reflect(skip_serializing)]
    pub source: Option<Entity>,
    /// Tick of application.
    pub applied_tick: u64,
    /// Ticks left (0 for `UntilRemoved`).
    pub remaining_ticks: u32,
    /// The full length in ticks (0 for `UntilRemoved`).
    pub duration_ticks: u32,
    /// Stack count.
    pub stacks: u16,
    /// Tick of the next pulse (0 without a pulse).
    pub next_pulse_tick: u64,
    /// Pulse period in ticks (0 without a pulse).
    pub period_ticks: u32,
    /// Amounts of `base_edits`, captured at application.
    pub edit_amounts: Vec<f32>,
    /// Amounts of `live_mods`, captured at application (`Live` stats are read every step).
    pub live_amounts: Vec<f32>,
    /// Caller parameters.
    pub params: Vec<(Tag, f32)>,
    /// Whether modifiers, status tags and pulses are stopped.
    pub suspended: bool,
    /// Suspended by `set_suspended`.
    pub held: bool,
    /// Grants made by this effect.
    pub granted: Vec<GrantId>,
    /// Client intent (reserved, § 2.8).
    pub intent: IntentSeq,
}

impl AppliedEffect {
    /// Makes this a copy of `other`, reusing this entry's buffers (the step path copies entries in
    /// and out of a scratch one). No `..` here: a new field must be added.
    fn copy_from(&mut self, other: &Self) {
        let Self {
            id,
            asset,
            level,
            source,
            applied_tick,
            remaining_ticks,
            duration_ticks,
            stacks,
            next_pulse_tick,
            period_ticks,
            edit_amounts,
            live_amounts,
            params,
            suspended,
            held,
            granted,
            intent,
        } = self;
        *id = other.id;
        asset.clone_from(&other.asset);
        *level = other.level;
        *source = other.source;
        *applied_tick = other.applied_tick;
        *remaining_ticks = other.remaining_ticks;
        *duration_ticks = other.duration_ticks;
        *stacks = other.stacks;
        *next_pulse_tick = other.next_pulse_tick;
        *period_ticks = other.period_ticks;
        edit_amounts.clone_from(&other.edit_amounts);
        live_amounts.clone_from(&other.live_amounts);
        params.clone_from(&other.params);
        *suspended = other.suspended;
        *held = other.held;
        granted.clone_from(&other.granted);
        *intent = other.intent;
    }
}

/// The live effects of one entity, sorted by [`EffectId`].
///
/// When the entity despawns, its live effects end with [`EndReason::TargetGone`] in the next effect
/// pass (spec 53 § 2.4): a despawn hook records them while the entity still exists.
#[derive(Component, Reflect, Debug, Clone, Default)]
#[reflect(Component, Default)]
#[component(on_despawn = target_despawned)]
pub struct AppliedEffects {
    /// Entries, ascending id.
    pub list: Vec<AppliedEffect>,
}

impl AppliedEffects {
    /// The entry `id`.
    #[must_use]
    pub fn get(&self, id: EffectId) -> Option<&AppliedEffect> {
        self.list.iter().find(|e| e.id == id)
    }

    /// Mutable entry `id`.
    pub fn get_mut(&mut self, id: EffectId) -> Option<&mut AppliedEffect> {
        self.list.iter_mut().find(|e| e.id == id)
    }
}

/// One live effect of a despawned target, recorded by the `AppliedEffects` despawn hook.
#[derive(Debug, Clone)]
pub(crate) struct GoneEffect {
    /// The target's § 7 order key, read while it still existed.
    key: super::OrderKey,
    target: Entity,
    id: EffectId,
    asset: String,
    source: Option<Entity>,
    stacks: u16,
}

/// `AppliedEffects` despawn hook: records the dying target's live effects for the next effect pass.
/// A plain `remove::<AppliedEffects>()` does not run it (the target still exists).
fn target_despawned(mut world: DeferredWorld<'_>, context: HookContext) {
    let target = context.entity;
    let key = super::order_key(target, world.get::<EntityGuid>(target));
    let gone: Vec<GoneEffect> = world
        .get::<AppliedEffects>(target)
        .map(|list| {
            list.list
                .iter()
                .map(|e| GoneEffect {
                    key,
                    target,
                    id: e.id,
                    asset: e.asset.clone(),
                    source: e.source,
                    stacks: e.stacks,
                })
                .collect()
        })
        .unwrap_or_default();
    if gone.is_empty() {
        return;
    }
    if let Some(mut pending) = world.get_resource_mut::<PendingChains>() {
        pending.1.extend(gone);
    }
}

/// Ends the effects of targets despawned since the last pass with [`EndReason::TargetGone`], in the
/// § 7 order (target key, then id). The target's tags, grants and attributes went with it, and its
/// chains have no target to apply to, so what is left is the message, the end cues (presentation may
/// be holding a sustained cue for the effect) and the counters.
fn end_gone_targets(world: &mut World) {
    let Some(mut gone) = world
        .get_resource_mut::<PendingChains>()
        .filter(|p| !p.1.is_empty())
        .map(|mut p| std::mem::take(&mut p.1))
    else {
        return;
    };
    gone.sort_by_key(|g| (g.key, g.id));
    for g in &gone {
        world.write_message(EffectEnded {
            target: g.target,
            id: g.id,
            reason: EndReason::TargetGone,
        });
        if let Some(asset) = assets::effect(world, &g.asset) {
            cues::fire_cues(
                world,
                &asset.cues,
                CueAt::Removed,
                Some(CueKind::End),
                CueBase {
                    entity: g.target,
                    source: g.source,
                    amount: f32::from(g.stacks),
                    effect: Some(g.id),
                },
            );
        }
        if let Some(mut stats) = world.get_resource_mut::<AbilityStats>() {
            stats.count_end(EndReason::TargetGone);
        }
    }
    // Give the buffer back so a later despawn does not allocate again.
    gone.clear();
    if let Some(mut pending) = world.get_resource_mut::<PendingChains>()
        && pending.1.is_empty()
    {
        pending.1 = gone;
    }
}

/// Counted tags granted by effects and running abilities: sorted `(tag, count)`.
#[derive(Component, Reflect, Debug, Clone, Default)]
#[reflect(Component, Default)]
pub struct TagCounts {
    counts: Vec<(Tag, u16)>,
    /// The effective set at the last `EffectiveTagsChanged` report.
    #[reflect(ignore)]
    pub(crate) reported: Option<Vec<Tag>>,
}

impl TagCounts {
    /// The count of `tag`.
    #[must_use]
    pub fn count(&self, tag: Tag) -> u16 {
        self.counts.iter().find(|(t, _)| *t == tag).map_or(0, |(_, n)| *n)
    }

    /// Every counted tag with its count, sorted by tag.
    #[must_use]
    pub fn iter(&self) -> &[(Tag, u16)] {
        &self.counts
    }

    /// Adds one count of `tag`.
    pub fn add(&mut self, tag: Tag) {
        match self.counts.binary_search_by(|(t, _)| t.cmp(&tag)) {
            Ok(i) => self.counts[i].1 = self.counts[i].1.saturating_add(1),
            Err(i) => self.counts.insert(i, (tag, 1)),
        }
    }

    /// Removes one count of `tag` (the entry leaves at zero).
    pub fn remove(&mut self, tag: Tag) {
        if let Ok(i) = self.counts.binary_search_by(|(t, _)| t.cmp(&tag)) {
            self.counts[i].1 = self.counts[i].1.saturating_sub(1);
            if self.counts[i].1 == 0 {
                self.counts.remove(i);
            }
        }
    }
}

/// The application outcome codes (§ 5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    /// A new entry (or an instant edit) took hold.
    Applied,
    /// Merged into an existing entry.
    Stacked,
    /// The chance roll failed.
    FailedChance,
    /// `apply_if` did not match.
    BlockedCondition,
    /// A live effect's immunity matched.
    BlockedImmunity,
    /// The entry is at its cap.
    RejectedFull,
    /// At the cap; another effect was applied instead.
    Spilled,
    /// The target holds the most live effects allowed.
    LimitReached,
    /// A chain went deeper than 4.
    ChainTooDeep,
}

impl Outcome {
    /// Lowercase code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Outcome::Applied => "applied",
            Outcome::Stacked => "stacked",
            Outcome::FailedChance => "failed_chance",
            Outcome::BlockedCondition => "blocked_condition",
            Outcome::BlockedImmunity => "blocked_immunity",
            Outcome::RejectedFull => "rejected_full",
            Outcome::Spilled => "spilled",
            Outcome::LimitReached => "limit_reached",
            Outcome::ChainTooDeep => "chain_too_deep",
        }
    }
}

/// The result of [`apply_effect`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// What happened.
    pub outcome: Outcome,
    /// The entry, when one exists.
    pub id: Option<EffectId>,
    /// Its stack count (0 for instant effects and refusals without an entry).
    pub stacks: u16,
}

impl ApplyOutcome {
    fn bare(outcome: Outcome) -> Self {
        Self {
            outcome,
            id: None,
            stacks: 0,
        }
    }
}

/// Why an effect ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndReason {
    /// Its time ran out.
    Expired,
    /// Removed by a call or query.
    Removed,
    /// Removed by another effect's `cleanses`.
    Cleansed,
    /// Ended by a stack restart.
    Restarted,
    /// Its source despawned (`end_with_source`).
    SourceGone,
    /// Its target despawned.
    TargetGone,
}

impl EndReason {
    /// Lowercase code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            EndReason::Expired => "expired",
            EndReason::Removed => "removed",
            EndReason::Cleansed => "cleansed",
            EndReason::Restarted => "restarted",
            EndReason::SourceGone => "source_gone",
            EndReason::TargetGone => "target_gone",
        }
    }
}

/// A request to apply an effect (also the [`apply_effect`] argument).
#[derive(Message, Debug, Clone, PartialEq)]
pub struct ApplyEffect {
    /// Effect asset path (a key of `AbilityAssets`).
    pub asset: String,
    /// Who applies it.
    pub source: Option<Entity>,
    /// Who receives it.
    pub target: Entity,
    /// Level (curves are sampled at it).
    pub level: u32,
    /// Caller parameters for `Param` amounts.
    pub params: SmallVec<[(Tag, f32); 4]>,
    /// Client intent (reserved).
    pub intent: IntentSeq,
}

impl ApplyEffect {
    /// A level-1 request without source or parameters.
    #[must_use]
    pub fn new(asset: &str, target: Entity) -> Self {
        Self {
            asset: asset.to_owned(),
            source: None,
            target,
            level: 1,
            params: SmallVec::new(),
            intent: IntentSeq(0),
        }
    }

    /// Builder: the source.
    #[must_use]
    pub fn from_source(mut self, source: Entity) -> Self {
        self.source = Some(source);
        self
    }

    /// Builder: the level.
    #[must_use]
    pub fn at_level(mut self, level: u32) -> Self {
        self.level = level;
        self
    }

    /// Builder: a parameter.
    #[must_use]
    pub fn with_param(mut self, key: Tag, value: f32) -> Self {
        self.params.push((key, value));
        self
    }
}

/// Written for every application attempt.
#[derive(Message, Debug, Clone, PartialEq)]
pub struct EffectApplied {
    /// The target.
    pub target: Entity,
    /// The asset path.
    pub asset: String,
    /// The entry, if any.
    pub id: Option<EffectId>,
    /// What happened.
    pub outcome: Outcome,
    /// Stack count.
    pub stacks: u16,
    /// Client intent.
    pub intent: IntentSeq,
}

/// Written when a live effect ends.
#[derive(Message, Debug, Clone, PartialEq)]
pub struct EffectEnded {
    /// The target.
    pub target: Entity,
    /// The entry.
    pub id: EffectId,
    /// Why.
    pub reason: EndReason,
}

/// Written when immunity refuses an application.
#[derive(Message, Debug, Clone, PartialEq)]
pub struct EffectRefused {
    /// The target.
    pub target: Entity,
    /// The refused asset.
    pub asset: String,
    /// The live effect whose immunity matched.
    pub by: EffectId,
}

/// Written on every stack count change.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct StacksChanged {
    /// The target.
    pub target: Entity,
    /// The entry.
    pub id: EffectId,
    /// Count before.
    pub old: u16,
    /// Count after.
    pub new: u16,
}

/// Written in `AbilitySet::Notify` when an entity's effective tags changed.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct EffectiveTagsChanged {
    /// The entity.
    pub entity: Entity,
    /// Tags gained, sorted.
    pub added: Vec<Tag>,
    /// Tags lost, sorted.
    pub removed: Vec<Tag>,
}

/// Chained applications waiting for the next effect pass, and the live effects of targets despawned
/// since the last effect pass (spec 53 § 2.4 `target_gone`: written by the `AppliedEffects` despawn
/// hook, drained at the start of [`effects_pass`]). One resource for both: in this ECS a resource is
/// an entity, and another one would shift the ids of every later spawn (ADR-7216).
#[derive(Resource, Debug, Clone, Default)]
pub(crate) struct PendingChains(pub(crate) Vec<(ApplyEffect, u8)>, pub(crate) Vec<GoneEffect>);

// ---------------------------------------------------------------------------------------------
// Effective tags.

/// The explicit `TagContainer` plus every counted tag.
#[must_use]
pub fn effective_container(world: &World, e: Entity) -> TagContainer {
    let mut c = world.get::<TagContainer>(e).cloned().unwrap_or_default();
    if let Some(counts) = world.get::<TagCounts>(e) {
        for (tag, n) in &counts.counts {
            if *n > 0 {
                c.add(*tag);
            }
        }
    }
    c
}

/// Whether `e`'s effective tags hold `tag` (hierarchy match, like `TagContainer::has`).
#[must_use]
pub fn effective_has(world: &World, e: Entity, tag: Tag) -> bool {
    world.get::<TagContainer>(e).is_some_and(|c| c.has(tag))
        || world
            .get::<TagCounts>(e)
            .is_some_and(|c| c.counts.iter().any(|(t, n)| *n > 0 && t.matches(tag)))
}

/// `e`'s effective tags as a [`TagLookup`], for checks that must not build a container.
#[derive(Clone, Copy)]
pub(crate) struct EffectiveTags<'a> {
    explicit: Option<&'a TagContainer>,
    counts: Option<&'a TagCounts>,
}

impl<'a> EffectiveTags<'a> {
    pub(crate) fn of(world: &'a World, e: Entity) -> Self {
        Self {
            explicit: world.get::<TagContainer>(e),
            counts: world.get::<TagCounts>(e),
        }
    }

    fn counted(&self) -> impl Iterator<Item = Tag> + 'a {
        let counts = self.counts.map_or(&[][..], |c| c.counts.as_slice());
        counts.iter().filter(|(_, n)| *n > 0).map(|(tag, _)| *tag)
    }
}

impl TagLookup for EffectiveTags<'_> {
    fn has_tag(&self, tag: Tag) -> bool {
        self.explicit.is_some_and(|c| c.has(tag)) || self.counted().any(|t| t.matches(tag))
    }

    fn has_tag_exact(&self, tag: Tag) -> bool {
        self.explicit.is_some_and(|c| c.has_exact(tag)) || self.counted().any(|t| t == tag)
    }
}

/// Fills `out` with `e`'s effective tags, sorted without duplicates.
pub(crate) fn effective_tags_into(world: &World, e: Entity, out: &mut Vec<Tag>) {
    let tags = EffectiveTags::of(world, e);
    out.clear();
    out.extend(tags.explicit.into_iter().flat_map(TagContainer::iter));
    out.extend(tags.counted());
    out.sort_unstable();
    out.dedup();
}

/// Whether `e`'s effective tags match `q`.
#[must_use]
pub fn effective_matches(world: &World, e: Entity, q: &TagQuery) -> bool {
    q.eval_on(&EffectiveTags::of(world, e))
}

/// Counts (`add`) or uncounts `tags` on `e`.
pub(crate) fn count_tags(world: &mut World, e: Entity, tags: &[Tag], add: bool) {
    if tags.is_empty() {
        return;
    }
    let Ok(mut entity) = world.get_entity_mut(e) else {
        return;
    };
    if entity.get::<TagCounts>().is_none() {
        // A snapshot of "nothing reported yet": the first notify reports every tag as added.
        // A `TagCounts` without one (a loaded save; `reported` is not saved) only takes its
        // baseline instead of re-reporting tags that did not change (ADR-1809).
        entity.insert(TagCounts {
            reported: Some(Vec::new()),
            ..TagCounts::default()
        });
    }
    if let Some(mut counts) = entity.get_mut::<TagCounts>() {
        for tag in tags {
            if add {
                counts.add(*tag);
            } else {
                counts.remove(*tag);
            }
        }
    }
    if let Some(mut attrs) = entity.get_mut::<Attributes>() {
        attrs.mark_dirty();
    }
}

// ---------------------------------------------------------------------------------------------
// Amounts.

pub(crate) fn warn(world: &mut World, code: &'static str) {
    tracing::debug!(target: "regia_framework", code, "abilities warning");
    if let Some(mut stats) = world.get_resource_mut::<AbilityStats>() {
        *stats.warnings.entry(code).or_insert(0) += 1;
    }
}

/// Who and what an amount is evaluated for.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Ctx<'a> {
    pub(crate) level: u32,
    pub(crate) stacks: u16,
    pub(crate) source: Option<Entity>,
    pub(crate) target: Entity,
    pub(crate) params: &'a [(Tag, f32)],
}

/// Resolves one amount. Warnings are pushed to `warnings`.
pub(crate) fn magnitude(
    world: &World,
    asset: &EffectAsset,
    m: &Magnitude,
    ctx: Ctx,
    warnings: &mut Vec<&'static str>,
) -> f32 {
    let Ctx {
        level,
        stacks,
        source,
        target,
        params,
    } = ctx;
    match m {
        Magnitude::Const(v) => *v,
        Magnitude::Curve(r) => {
            let x = level as f32;
            let found = if r.table.is_empty() {
                asset
                    .curves
                    .iter()
                    .find(|c| c.name == r.name)
                    .and_then(|c| c.sample(x))
            } else {
                world
                    .get_resource::<crate::curve_table::CurveTables>()
                    .and_then(|t| LevelCurves::sample(t, &r.table, &r.name, x))
                    .or_else(|| {
                        world
                            .get_resource::<LevelCurvesRes>()
                            .and_then(|c| c.0.sample(&r.table, &r.name, x))
                    })
            };
            found.unwrap_or_else(|| {
                warnings.push("curve_missing");
                0.0
            })
        }
        Magnitude::Stat {
            side,
            attribute,
            weight,
            offset,
            ..
        } => {
            let who = match side {
                StatSide::Source => source,
                StatSide::Target => Some(target),
            };
            match who
                .and_then(|w| world.get::<Attributes>(w))
                .and_then(|a| a.get(AttrId::of(attribute)))
            {
                Some(v) => offset + weight * v,
                None => {
                    warnings.push("unknown_attribute");
                    *offset
                }
            }
        }
        Magnitude::Param { key, default } => match params.iter().find(|(k, _)| k == key) {
            Some((_, v)) => *v,
            None => {
                warnings.push("param_missing");
                *default
            }
        },
        Magnitude::Function(name) => match world.get_resource::<MagnitudeFns>().and_then(|f| f.get(name)) {
            Some(f) => {
                let input = MagnitudeInput {
                    source: source.and_then(|s| world.get::<Attributes>(s)),
                    target: world.get::<Attributes>(target),
                    level,
                    stacks,
                };
                let v = f(&input);
                if v.is_finite() {
                    v
                } else {
                    warnings.push("attribute_not_finite");
                    0.0
                }
            }
            None => {
                warnings.push("magnitude_fn_missing");
                0.0
            }
        },
    }
}

/// Captured amounts of `base_edits` and `live_mods` for `entry`. The returned flag is `true`
/// when the evaluation already logged `unknown_attribute` (a `Magnitude::Stat` amount reading a
/// missing attribute), so a caller's own missing-attribute check can skip its warning and keep
/// § 4.5's once-per-application rule.
pub(crate) fn evaluate_amounts(
    world: &mut World,
    asset: &EffectAsset,
    entry: &AppliedEffect,
    target: Entity,
) -> (Vec<f32>, Vec<f32>, bool) {
    let mut warnings = Vec::new();
    let w: &World = world;
    let ctx = Ctx {
        level: entry.level,
        stacks: entry.stacks,
        source: entry.source,
        target,
        params: &entry.params,
    };
    let eval = |list: &[Modifier], warnings: &mut Vec<&'static str>| -> Vec<f32> {
        list.iter()
            .map(|m| magnitude(w, asset, &m.amount, ctx, warnings))
            .collect()
    };
    let edits = eval(&asset.base_edits, &mut warnings);
    let live = eval(&asset.live_mods, &mut warnings);
    let warned_unknown_attribute = warnings.contains(&"unknown_attribute");
    for w in warnings {
        warn(world, w);
    }
    (edits, live, warned_unknown_attribute)
}

/// Ticks for a duration in seconds. Non-finite products and products `<= 1` are 1.
/// A product within `1e-4` of an integer uses that integer; otherwise the value is
/// `ceil`. `0.1 * 60` is 6 ticks.
#[must_use]
pub fn ticks_from_seconds(seconds: f32, rate: f64) -> u32 {
    let x = f64::from(seconds) * rate;
    if !x.is_finite() || x <= 1.0 {
        return 1;
    }
    let near = x.round();
    let t = if (x - near).abs() <= 1e-4 { near } else { x.ceil() };
    t.min(f64::from(u32::MAX)) as u32
}

/// The fixed-step rate (`FixedTime::hz`, 60 without one).
pub(crate) fn tick_rate(world: &World) -> f64 {
    world.get_resource::<FixedTime>().map_or(60.0, |f| f.hz)
}

/// The timed length of `asset` in ticks for this request (0 for other lifetimes).
pub(crate) fn length_ticks(world: &mut World, asset: &EffectAsset, req: &ApplyEffect, stacks: u16) -> u32 {
    let Lifetime::Timed(m) = &asset.lifetime else {
        return 0;
    };
    let mut warnings = Vec::new();
    let ctx = Ctx {
        level: req.level,
        stacks,
        source: req.source,
        target: req.target,
        params: &req.params,
    };
    let seconds = magnitude(world, asset, m, ctx, &mut warnings);
    for w in warnings {
        warn(world, w);
    }
    ticks_from_seconds(seconds, tick_rate(world))
}

// ---------------------------------------------------------------------------------------------
// Base edits.

/// One batch of instant base edits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Edit<'a> {
    pub(crate) target: Entity,
    pub(crate) source: Option<Entity>,
    pub(crate) effect: Option<EffectId>,
    pub(crate) mods: &'a [Modifier],
    pub(crate) amounts: &'a [f32],
    pub(crate) stacks: u16,
    /// Check only: nothing is written.
    pub(crate) dry: bool,
}

/// Bases of a planned edit. On the stack up to the § 10 budget of attributes per entity, so a pulse
/// and a dry run allocate nothing (§ 9).
pub(crate) type PlannedBases = SmallVec<[f32; 64]>;

/// Runs `edit.mods` with `edit.amounts` as instant edits of the target's base through the edit
/// gates. With `dry`, nothing is written and `Err(attribute)` names the first edit that would take
/// a base below its minimum.
pub(crate) fn edit_base(world: &mut World, edit: Edit) -> Result<(), AttrId> {
    let mut bases = PlannedBases::new();
    let (result, planned, warnings) = plan_edits(world, edit, &mut bases);
    if edit.dry {
        return result;
    }
    for w in warnings {
        warn(world, w);
    }
    if planned && let Some(mut stored) = world.get_mut::<Attributes>(edit.target) {
        for (attribute, base) in stored.list_mut().iter_mut().zip(&bases) {
            attribute.base = *base;
        }
        stored.clean = false;
        stored.last_source = edit.source;
    }
    result
}

/// Fills `bases` with the target's bases after the edit, without writing them; the middle value
/// says whether the target has attributes at all.
pub(crate) fn plan_edits(
    world: &World,
    edit: Edit,
    bases: &mut PlannedBases,
) -> (Result<(), AttrId>, bool, Vec<&'static str>) {
    let Edit {
        target,
        source,
        effect,
        mods,
        amounts,
        stacks,
        dry,
    } = edit;
    let Some(attrs) = world.get::<Attributes>(target) else {
        return (Ok(()), false, Vec::new());
    };
    bases.clear();
    bases.extend(attrs.list().iter().map(|a| a.base));
    let mut warnings: Vec<&'static str> = Vec::new();
    let mut result = Ok(());
    let gates = world.get_resource::<EditGates>();
    for (m, amount) in mods.iter().zip(amounts) {
        let id = AttrId::of(&m.attribute);
        let Some(index) = attrs.index_of(id) else {
            // Once per application (§ 4.5), however many modifiers name missing attributes.
            if !warnings.contains(&"unknown_attribute") {
                warnings.push("unknown_attribute");
            }
            continue;
        };
        if attrs.list()[index].derived.is_some() {
            warnings.push("attribute_derived");
            continue;
        }
        let live = LiveMod {
            effect: effect.unwrap_or(EffectId(0)),
            applied_tick: 0,
            op: m.op,
            amount: *amount,
            stacks,
            per_stack: m.per_stack,
        };
        let old = bases[index];
        let proposed = m.op.apply_to_base(old, live.stacked());
        if !proposed.is_finite() {
            warnings.push("attribute_not_finite");
            continue;
        }
        let request = super::EditRequest {
            target,
            attribute: id,
            delta: proposed - old,
            source,
            effect,
        };
        let delta = match gates {
            Some(g) => g.run(world, &request),
            None => Some(request.delta),
        };
        let Some(delta) = delta else {
            continue;
        };
        let new = old + delta;
        if !new.is_finite() {
            warnings.push("attribute_not_finite");
            continue;
        }
        let min = attrs.list()[index].min;
        if dry && new < min && delta < 0.0 {
            result = Err(id);
            break;
        }
        bases[index] = new.clamp(min, attrs.effective_max(index));
    }
    (result, true, warnings)
}

// ---------------------------------------------------------------------------------------------
// Application.

/// Applies an effect now (§ 2.4 application order). Refusals are outcomes, not errors.
pub fn apply_effect(world: &mut World, req: ApplyEffect) -> Result<ApplyOutcome, AbilityError> {
    let _span = tracing::info_span!(target: "regia_framework", "abilities_apply_effect").entered();
    apply_effect_at_depth(world, req, 0)
}

/// [`apply_effect`] at chain depth `depth`; writes [`EffectApplied`] for every outcome.
pub(crate) fn apply_effect_at_depth(
    world: &mut World,
    req: ApplyEffect,
    depth: u8,
) -> Result<ApplyOutcome, AbilityError> {
    let asset_path = req.asset.clone();
    let target = req.target;
    let intent = req.intent;
    let out = apply_inner(world, req, depth)?;
    if matches!(out.outcome, Outcome::Applied | Outcome::Stacked)
        && let Some(mut stats) = world.get_resource_mut::<AbilityStats>()
    {
        stats.applications += 1;
    }
    world.write_message(EffectApplied {
        target,
        asset: asset_path,
        id: out.id,
        outcome: out.outcome,
        stacks: out.stacks,
        intent,
    });
    Ok(out)
}

thread_local! {
    static REPLAY_STEPS: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

/// How many `Rng` steps the chance rolls on this thread have replayed to rebuild a stream position
/// (the slow path of `roll`, used before a stream is first advanced and after a load or a host
/// edit of [`AbilityCounters`]). Like [`super::activation_check_count`], this is a deterministic
/// observable for budget tests: no wall time.
#[must_use]
pub fn chance_replay_steps() -> u64 {
    REPLAY_STEPS.with(|steps| steps.get())
}

/// The chance-stream generators of the rows of [`AbilityCounters::rolls`], kept advanced in place
/// so a roll costs O(1) instead of replaying its saved position through the RNG (deep-zai
/// finding 2: a long game would otherwise replay linearly per roll and quadratically in
/// aggregate inside `FixedUpdate`).
///
/// Transient, never saved: an entry names the position its generator is good for, and a mismatch
/// (a load, a host edit of the counters) falls back to the replay path once and re-caches. The
/// drawn values are exactly the replay's, so determinism and the § 7 state hash do not change.
#[derive(Resource, Default)]
pub(crate) struct RollStreams {
    /// `(target key, asset id)` to (the position the generator is good for, the generator after
    /// the last draw).
    streams: std::collections::HashMap<(u64, u64), (u64, regia_core::math::Rng)>,
}

impl RollStreams {
    /// The generator for `(key, asset)` when it is good for `position` (its state after that many
    /// draws); `None` when it is absent or stale and must be rebuilt by replay.
    fn take(&mut self, key: u64, asset: u64, position: u64) -> Option<regia_core::math::Rng> {
        match self.streams.remove(&(key, asset)) {
            Some((next, rng)) if next == position => Some(rng),
            _ => None,
        }
    }

    /// Stores `rng` as the generator for `(key, asset)`, good for `position`.
    fn put(&mut self, key: u64, asset: u64, position: u64, rng: regia_core::math::Rng) {
        self.streams.insert((key, asset), (position, rng));
    }

    /// Drops the cached generators whose target key left the world (the same live-key rule the
    /// prune of [`AbilityCounters::rolls`] just applied to the rows).
    fn retain_live(&mut self, live: &[u64]) {
        self.streams.retain(|(key, _), _| live.binary_search(key).is_ok());
    }
}

/// The chance roll for `(target, asset)`: advances the saved stream position once. `None` when the
/// target has no `EntityGuid`: the caller refuses deterministically, so a missing guid cannot
/// change accept versus reject between spawn orders (audit G finding 5, ADR-4756).
fn roll(world: &mut World, target: Entity, asset: &str) -> Option<f32> {
    let key = super::target_key(world, target)?;
    let asset_id = crate::tags::fnv1a64(asset.as_bytes());
    let seed = world.get_resource::<AbilitySettings>().map_or(0, |s| s.seed);
    let position = world
        .get_resource_mut::<AbilityCounters>()
        .map_or(0, |mut c| c.next_roll(key, asset_id));
    let mut rng = match world
        .get_resource_mut::<RollStreams>()
        .and_then(|mut streams| streams.take(key, asset_id, position))
    {
        Some(rng) => rng,
        None => {
            let mut rng = regia_core::math::Rng::with_stream(seed ^ key, asset_id);
            for _ in 0..position {
                rng.next_u32();
                REPLAY_STEPS.with(|steps| steps.set(steps.get() + 1));
            }
            rng
        }
    };
    let value = rng.f32();
    if let Some(mut streams) = world.get_resource_mut::<RollStreams>() {
        streams.put(key, asset_id, position + 1, rng);
    }
    Some(value)
}

fn apply_inner(world: &mut World, req: ApplyEffect, depth: u8) -> Result<ApplyOutcome, AbilityError> {
    let asset =
        assets::effect(world, &req.asset).ok_or_else(|| AbilityError::EffectNotFound(req.asset.clone()))?;
    if world.get_entity(req.target).is_err() {
        return Err(AbilityError::EntityNotFound);
    }
    if depth > MAX_CHAIN_DEPTH {
        warn(world, "chain_too_deep");
        return Ok(ApplyOutcome::bare(Outcome::ChainTooDeep));
    }
    // 1. Chance.
    if asset.chance < 1.0 && roll(world, req.target, &req.asset).is_none_or(|r| r >= asset.chance) {
        return Ok(ApplyOutcome::bare(Outcome::FailedChance));
    }
    // 2. Condition.
    let effective = effective_container(world, req.target);
    if let Some(q) = &asset.apply_if
        && !effective.matches(q)
    {
        return Ok(ApplyOutcome::bare(Outcome::BlockedCondition));
    }
    // 3. Immunity.
    let kind = asset.kind_container();
    if let Some(by) = immune_by(world, req.target, &kind) {
        world.write_message(EffectRefused {
            target: req.target,
            asset: req.asset.clone(),
            by,
        });
        return Ok(ApplyOutcome::bare(Outcome::BlockedImmunity));
    }
    // 4. Stacking.
    if !asset.is_instant() {
        if let Some(rule) = &asset.stack {
            match stacking::try_merge(world, &req, &asset, rule, depth)? {
                Merge::Done(out) => return Ok(out),
                Merge::New | Merge::Fresh => {}
            }
        }
        let live = world
            .get::<AppliedEffects>(req.target)
            .map_or(0, |e| e.list.len());
        let total = world.get_resource::<AbilityStats>().map_or(0, |s| s.live_effects);
        if live >= MAX_EFFECTS_PER_ENTITY || total >= MAX_EFFECTS_TOTAL {
            return Ok(ApplyOutcome::bare(Outcome::LimitReached));
        }
    }
    // 5. Amounts.
    let tick = world.get_resource::<AbilityCounters>().map_or(0, |c| c.tick);
    let mut entry = AppliedEffect {
        id: EffectId(0),
        asset: req.asset.clone(),
        level: req.level,
        source: req.source,
        applied_tick: tick,
        remaining_ticks: 0,
        duration_ticks: 0,
        stacks: 1,
        next_pulse_tick: 0,
        period_ticks: 0,
        edit_amounts: Vec::new(),
        live_amounts: Vec::new(),
        params: req.params.to_vec(),
        suspended: false,
        held: false,
        granted: Vec::new(),
        intent: req.intent,
    };
    let (edits, live, warned_unknown_attribute) = evaluate_amounts(world, &asset, &entry, req.target);
    entry.edit_amounts = edits;
    entry.live_amounts = live;
    // 6. Cleanses, then the effect takes hold.
    for q in &asset.cleanses {
        remove_matching_with(world, req.target, q, None, EndReason::Cleansed);
    }
    let base = CueBase {
        entity: req.target,
        source: req.source,
        amount: 1.0,
        effect: None,
    };
    if asset.is_instant() {
        let _ = edit_base(
            world,
            Edit {
                target: req.target,
                source: req.source,
                effect: None,
                mods: &asset.base_edits,
                amounts: &entry.edit_amounts,
                stacks: 1,
                dry: false,
            },
        );
        queue_chain(world, &asset.chain.on_apply, &req, depth + 1);
        cues::fire_cues(world, &asset.cues, CueAt::Applied, None, base);
        return Ok(ApplyOutcome::bare(Outcome::Applied));
    }
    let id = world
        .get_resource_mut::<AbilityCounters>()
        .map_or(EffectId(0), |mut c| c.next_effect_id());
    entry.id = id;
    let length = length_ticks(world, &asset, &req, 1);
    entry.remaining_ticks = length;
    entry.duration_ticks = length;
    if let Some(p) = asset.pulse {
        entry.period_ticks = ticks_from_seconds(p.every, tick_rate(world));
        entry.next_pulse_tick = tick + u64::from(entry.period_ticks);
    }
    entry.suspended = asset.active_if.as_ref().is_some_and(|q| !effective.matches(q));
    let suspended = entry.suspended;
    let mut pushed = false;
    let mut over_soft_limit = false;
    if let Ok(mut e) = world.get_entity_mut(req.target) {
        if e.get::<AppliedEffects>().is_none() {
            e.insert(AppliedEffects::default());
        }
        if let Some(mut list) = e.get_mut::<AppliedEffects>() {
            list.list.push(entry.clone());
            pushed = true;
            over_soft_limit = list.list.len() > SOFT_EFFECTS_PER_ENTITY;
        }
        if let Some(mut attrs) = e.get_mut::<Attributes>() {
            attrs.mark_dirty();
        }
    }
    // The world cap reads this between recomputes: keep it current within the step
    // (the recompute pass still resyncs it to the exact count; ADR-1809).
    if over_soft_limit {
        warn(world, "effects_over_soft_limit");
    }
    if pushed && let Some(mut stats) = world.get_resource_mut::<AbilityStats>() {
        stats.live_effects += 1;
    }
    // A live modifier naming an attribute the target lacks is skipped by every recompute; the
    // application logs it once (§ 4.5), like a base edit does. A suspended effect applies no
    // modifiers (collect_mods filters it), and an amount read that already warned (a
    // `Magnitude::Stat` on the same missing attribute) must not warn a second time.
    if !suspended
        && !warned_unknown_attribute
        && world.get::<Attributes>(req.target).is_some_and(|attrs| {
            asset
                .live_mods
                .iter()
                .any(|m| attrs.index_of(AttrId::of(&m.attribute)).is_none())
        })
    {
        warn(world, "unknown_attribute");
    }
    if !suspended && asset.pulse.is_some_and(|p| p.on_apply) {
        run_pulse(world, req.target, &entry, &asset);
    }
    // 7. Tags, grants, chain.
    if !suspended {
        count_tags(world, req.target, &asset.status_tags, true);
    }
    let mut granted = Vec::new();
    for g in &asset.grants {
        if let Ok(gid) = grants::grant_ability(
            world,
            req.target,
            &g.ability,
            GrantOptions {
                level: g.level,
                ..GrantOptions::default()
            },
        ) {
            granted.push(gid);
        }
    }
    if !granted.is_empty()
        && let Some(mut list) = world.get_mut::<AppliedEffects>(req.target)
        && let Some(e) = list.get_mut(id)
    {
        e.granted = granted;
    }
    queue_chain(world, &asset.chain.on_apply, &req, depth + 1);
    // 8. Cues (the message is written by the caller).
    let base = CueBase {
        effect: Some(id),
        ..base
    };
    cues::fire_cues(world, &asset.cues, CueAt::Applied, Some(CueKind::Begin), base);
    Ok(ApplyOutcome {
        outcome: Outcome::Applied,
        id: Some(id),
        stacks: 1,
    })
}

fn immune_by(world: &World, target: Entity, kind: &TagContainer) -> Option<EffectId> {
    let effects = world.get::<AppliedEffects>(target)?;
    effects.list.iter().find_map(|e| {
        let asset = assets::effect(world, &e.asset)?;
        asset.immunity.iter().any(|q| kind.matches(q)).then_some(e.id)
    })
}

fn queue_chain(world: &mut World, paths: &[String], req: &ApplyEffect, depth: u8) {
    if paths.is_empty() {
        return;
    }
    let Some(mut pending) = world.get_resource_mut::<PendingChains>() else {
        return;
    };
    for path in paths {
        pending.0.push((
            ApplyEffect {
                asset: path.clone(),
                ..req.clone()
            },
            depth,
        ));
    }
}

/// Applies every queued chained effect, including the ones those queue (bounded by depth).
pub(crate) fn drain_chains(world: &mut World) {
    for _ in 0..=usize::from(MAX_CHAIN_DEPTH) + 1 {
        let batch = world
            .get_resource_mut::<PendingChains>()
            .map(|mut p| std::mem::take(&mut p.0))
            .unwrap_or_default();
        if batch.is_empty() {
            return;
        }
        for (req, depth) in batch {
            if world.get_entity(req.target).is_err() {
                continue;
            }
            let _ = apply_effect_at_depth(world, req, depth);
        }
    }
}

fn run_pulse(world: &mut World, target: Entity, entry: &AppliedEffect, asset: &EffectAsset) {
    let _ = edit_base(
        world,
        Edit {
            target,
            source: entry.source,
            effect: Some(entry.id),
            mods: &asset.base_edits,
            amounts: &entry.edit_amounts,
            stacks: entry.stacks,
            dry: false,
        },
    );
    cues::fire_cues(
        world,
        &asset.cues,
        CueAt::Pulsed,
        None,
        CueBase {
            entity: target,
            source: entry.source,
            amount: entry.edit_amounts.first().copied().unwrap_or(0.0),
            effect: Some(entry.id),
        },
    );
}

// ---------------------------------------------------------------------------------------------
// Removal.

/// Ends the live effect `id` on `target` with `reason`. Returns whether it existed.
pub(crate) fn end_effect(world: &mut World, target: Entity, id: EffectId, reason: EndReason) -> bool {
    let Some(entry) = world.get_mut::<AppliedEffects>(target).and_then(|mut list| {
        let index = list.list.iter().position(|e| e.id == id)?;
        Some(list.list.remove(index))
    }) else {
        return false;
    };
    let asset = assets::effect(world, &entry.asset);
    if let Some(asset) = &asset {
        if !entry.suspended {
            count_tags(world, target, &asset.status_tags, false);
        }
        // `granted` holds only the grants that succeeded, so it is matched to the asset's grants by
        // ability path (first unused entry), not by position: a failed earlier grant must not shift
        // a later grant onto the wrong end policy (ADR-1809).
        let mut used = vec![false; asset.grants.len()];
        for gid in &entry.granted {
            let Some(path) = world
                .get::<grants::AbilityBook>(target)
                .and_then(|book| book.get(*gid))
                .map(|grant| grant.asset.clone())
            else {
                continue;
            };
            let Some(at) = (0..asset.grants.len()).find(|&i| !used[i] && asset.grants[i].ability == path)
            else {
                continue;
            };
            used[at] = true;
            let g = &asset.grants[at];
            let _ = match g.on_effect_end {
                OnEffectEnd::Cancel => grants::revoke_ability(world, target, *gid, false),
                OnEffectEnd::AfterRun => grants::revoke_ability(world, target, *gid, true),
                OnEffectEnd::Keep => Ok(()),
            };
        }
        let req = ApplyEffect {
            asset: String::new(),
            source: entry.source,
            target,
            level: entry.level,
            params: entry.params.iter().copied().collect(),
            intent: entry.intent,
        };
        let chain = if reason == EndReason::Expired {
            &asset.chain.on_lapse
        } else {
            &asset.chain.on_removed
        };
        queue_chain(world, chain, &req, 1);
    }
    if let Some(mut attrs) = world.get_mut::<Attributes>(target) {
        attrs.mark_dirty();
    }
    world.write_message(EffectEnded { target, id, reason });
    if let Some(asset) = &asset {
        cues::fire_cues(
            world,
            &asset.cues,
            CueAt::Removed,
            Some(CueKind::End),
            CueBase {
                entity: target,
                source: entry.source,
                amount: f32::from(entry.stacks),
                effect: Some(id),
            },
        );
    }
    if let Some(mut stats) = world.get_resource_mut::<AbilityStats>() {
        stats.count_end(reason);
    }
    true
}

/// Removes the live effect `id` (reason `removed`).
pub fn remove_effect(world: &mut World, target: Entity, id: EffectId) -> Result<(), AbilityError> {
    if world.get_entity(target).is_err() {
        return Err(AbilityError::EntityNotFound);
    }
    if end_effect(world, target, id, EndReason::Removed) {
        Ok(())
    } else {
        Err(AbilityError::EffectIdNotFound(id.0))
    }
}

fn remove_matching_with(
    world: &mut World,
    target: Entity,
    q: &TagQuery,
    limit: Option<u32>,
    reason: EndReason,
) -> u32 {
    let ids: Vec<EffectId> = world
        .get::<AppliedEffects>(target)
        .map(|list| {
            list.list
                .iter()
                .filter(|e| assets::effect(world, &e.asset).is_some_and(|a| a.kind_container().matches(q)))
                .map(|e| e.id)
                .collect()
        })
        .unwrap_or_default();
    let mut removed = 0;
    for id in ids {
        if limit.is_some_and(|l| removed >= l) {
            break;
        }
        if end_effect(world, target, id, reason) {
            removed += 1;
        }
    }
    removed
}

/// Removes live effects whose `kind_tags` match `q` (reason `removed`), at most `limit`, in
/// ascending id. Returns how many ended.
pub fn remove_effects_matching(world: &mut World, target: Entity, q: &TagQuery, limit: Option<u32>) -> u32 {
    remove_matching_with(world, target, q, limit, EndReason::Removed)
}

/// Holds (or releases) a suspension of the live effect `id` from game code.
pub fn set_suspended(world: &mut World, target: Entity, id: EffectId, on: bool) -> Result<(), AbilityError> {
    let mut list = world
        .get_mut::<AppliedEffects>(target)
        .ok_or(AbilityError::EffectIdNotFound(id.0))?;
    let entry = list.get_mut(id).ok_or(AbilityError::EffectIdNotFound(id.0))?;
    entry.held = on;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The effect pass (AbilitySet::Effects).

/// Buffers of the effect pass, kept between steps.
#[derive(Default)]
pub(crate) struct EffectsScratch {
    order: EntityOrder,
    ids: Vec<EffectId>,
    /// `(position hint, id)` of the timed effects in a quiet run whose clock moves.
    ticks: Vec<(usize, EffectId)>,
    entry: Option<AppliedEffect>,
}

/// `FixedUpdate`, `AbilitySet::Effects`: chained effects, suspension, pulses, expiry, lapses.
pub(crate) fn effects_pass(
    world: &mut World,
    query: &mut QueryState<OrderItem, With<AppliedEffects>>,
    mut scratch: Local<EffectsScratch>,
) {
    let _span = tracing::info_span!(target: "regia_framework", "abilities_effects").entered();
    end_gone_targets(world);
    drain_chains(world);
    let tick = world.get_resource::<AbilityCounters>().map_or(0, |c| c.tick);
    let scratch = &mut *scratch;
    let entities = scratch.order.take(world, query);
    for &e in &entities {
        scratch.ids.clear();
        if let Some(list) = world.get::<AppliedEffects>(e) {
            scratch.ids.extend(list.list.iter().map(|x| x.id));
        }
        // Most live effects have nothing to do in a step, or only move their clock: each run of
        // them up to the next one that does more is tested on one borrow of the list and the
        // assets, and its clocks move in one write (spec 53 § 10 budget). A quiet effect writes
        // only its own clock, which no other effect's test reads, so this reads and writes what
        // one test and one write per effect did, in the same order.
        let mut at = 0;
        loop {
            scratch.ticks.clear();
            let busy = scratch
                .ids
                .get(at..)
                .and_then(|rest| quiet_run(world, e, rest, at, tick, &mut scratch.ticks));
            move_clocks(world, e, &scratch.ticks);
            let Some(busy) = busy else {
                break;
            };
            let Some(&id) = scratch.ids.get(at + busy) else {
                break;
            };
            step_effect(world, e, id, tick, &mut scratch.entry);
            at += busy + 1;
        }
    }
    scratch.order.give_back(entities);
    drain_chains(world);
}

/// What [`step_effect`] does with an effect this step, decided on borrows of the world.
enum QuietStep {
    /// The effect or its asset is gone: nothing to do.
    Gone,
    /// The usual step of a live effect: nothing happens.
    Idle,
    /// The usual step of a timed live effect: its clock moves and nothing else happens.
    Tick,
    /// Something happens: the full step.
    Full,
}

/// The test at the head of [`step_effect`], with the asset borrowed instead of shared out (no
/// reference count moves): it runs for every live effect every step (spec 53 § 10 budget). It reads
/// exactly what the full step's own test reads, and any case the full step treats specially is
/// [`QuietStep::Full`].
fn quiet_step(world: &World, e: Entity, id: EffectId, tick: u64) -> QuietStep {
    let (Some(list), Some(assets)) = (
        world.get::<AppliedEffects>(e),
        world.get_resource::<assets::AbilityAssets>(),
    ) else {
        return QuietStep::Gone;
    };
    quiet_step_in(world, list, assets, e, id, None, tick)
}

/// The offset in `ids` of the first effect whose [`quiet_step`] is `Full`, testing them all on
/// one borrow of `e`'s list and the assets; `None` when there is none. The `Tick` ones before it go
/// to `ticks`. `ids[0]` was at `first_position` in the list when the step began, the usual place
/// to find it.
fn quiet_run(
    world: &World,
    e: Entity,
    ids: &[EffectId],
    first_position: usize,
    tick: u64,
    ticks: &mut Vec<(usize, EffectId)>,
) -> Option<usize> {
    let list = world.get::<AppliedEffects>(e)?;
    let assets = world.get_resource::<assets::AbilityAssets>()?;
    ids.iter().enumerate().position(|(offset, &id)| {
        let position = first_position + offset;
        match quiet_step_in(world, list, assets, e, id, Some(position), tick) {
            QuietStep::Gone | QuietStep::Idle => false,
            QuietStep::Tick => {
                ticks.push((position, id));
                false
            }
            QuietStep::Full => true,
        }
    })
}

/// The `QuietStep::Tick` of each of `ticks`, in one write of `e`'s list. The write has a change
/// tick no other write has (the world's tick moves on either side of it), and `e`'s `Attributes`
/// records it with the list's tick before it (`Attributes::clock_write`): so the recompute pass can
/// tell that nothing but clocks moved, which no modifier reads, while the write still counts as a
/// change for everything else that watches the list (replication, hosts).
fn move_clocks(world: &mut World, e: Entity, ticks: &[(usize, EffectId)]) {
    if ticks.is_empty() {
        return;
    }
    let Some(before) = world
        .get_entity(e)
        .ok()
        .and_then(|entity| entity.get_change_ticks::<AppliedEffects>())
        .map(|t| t.changed)
    else {
        return;
    };
    world.increment_change_tick();
    let written = world.change_tick();
    if let Some(mut list) = world.get_mut::<AppliedEffects>(e) {
        for &(position, id) in ticks {
            let at = match list.list.get(position) {
                Some(entry) if entry.id == id => Some(position),
                _ => list.list.iter().position(|x| x.id == id),
            };
            if let Some(entry) = at.and_then(|at| list.list.get_mut(at)) {
                entry.remaining_ticks -= 1;
            }
        }
    }
    world.increment_change_tick();
    if let Some(mut attrs) = world.get_mut::<Attributes>(e) {
        // Bookkeeping of the recompute pass, not a change of the attributes.
        attrs.bypass_change_detection().clock_write = Some((before, written));
    }
}

/// [`quiet_step`] on borrowed parts; `position` is where the entry is likely to be in the list.
fn quiet_step_in(
    world: &World,
    list: &AppliedEffects,
    assets: &assets::AbilityAssets,
    e: Entity,
    id: EffectId,
    position: Option<usize>,
    tick: u64,
) -> QuietStep {
    let at_position = position.and_then(|at| list.list.get(at)).filter(|x| x.id == id);
    let Some(found) = at_position.or_else(|| list.get(id)) else {
        return QuietStep::Gone;
    };
    let Some(asset) = assets.effect_entry(&found.asset).map(|entry| &*entry.asset) else {
        return QuietStep::Gone;
    };
    if asset.end_with_source && found.source.is_some_and(|s| world.get_entity(s).is_err()) {
        return QuietStep::Full;
    }
    let should = found.held
        || asset
            .active_if
            .as_ref()
            .is_some_and(|q| !effective_matches(world, e, q));
    let ticking =
        matches!(asset.lifetime, Lifetime::Timed(_)) && !(found.suspended && asset.suspend.freeze_clock);
    let pulse_due = !found.suspended && found.period_ticks > 0 && found.next_pulse_tick <= tick;
    if should == found.suspended
        && found.applied_tick != tick
        && !pulse_due
        && (!ticking || found.remaining_ticks > 1)
    {
        if ticking { QuietStep::Tick } else { QuietStep::Idle }
    } else {
        QuietStep::Full
    }
}

fn step_effect(world: &mut World, e: Entity, id: EffectId, tick: u64, spare: &mut Option<AppliedEffect>) {
    match quiet_step(world, e, id, tick) {
        QuietStep::Gone | QuietStep::Idle => return,
        QuietStep::Tick => {
            if let Some(mut list) = world.get_mut::<AppliedEffects>(e)
                && let Some(entry) = list.get_mut(id)
            {
                entry.remaining_ticks -= 1;
            }
            return;
        }
        QuietStep::Full => {}
    }
    let Some(found) = world.get::<AppliedEffects>(e).and_then(|l| l.get(id)) else {
        return;
    };
    let Some(asset) = assets::effect(world, &found.asset) else {
        return;
    };
    if asset.end_with_source && found.source.is_some_and(|s| world.get_entity(s).is_err()) {
        end_effect(world, e, id, EndReason::SourceGone);
        return;
    }
    let should = found.held
        || asset
            .active_if
            .as_ref()
            .is_some_and(|q| !effective_matches(world, e, q));
    // `quiet_step` has taken the usual step of a live effect; from here something happens.
    let entry = match spare {
        Some(entry) => {
            entry.copy_from(found);
            entry
        }
        None => spare.insert(found.clone()),
    };
    // Suspension.
    if should != entry.suspended {
        entry.suspended = should;
        store(world, e, entry);
        count_tags(world, e, &asset.status_tags, !should);
        if !should && entry.period_ticks > 0 {
            match asset.suspend.on_resume {
                OnResume::Continue => {}
                OnResume::Restart => entry.next_pulse_tick = tick + u64::from(entry.period_ticks),
                OnResume::FireAndRestart => {
                    run_pulse(world, e, entry, &asset);
                    entry.next_pulse_tick = tick + u64::from(entry.period_ticks);
                }
            }
        }
    }
    if entry.applied_tick == tick {
        store(world, e, entry);
        return;
    }
    // Pulses.
    if !entry.suspended && entry.period_ticks > 0 {
        let mut runs = 0;
        while entry.next_pulse_tick <= tick && runs < MAX_PULSES_PER_STEP {
            run_pulse(world, e, entry, &asset);
            entry.next_pulse_tick += u64::from(entry.period_ticks);
            runs += 1;
        }
        if entry.next_pulse_tick <= tick {
            warn(world, "pulse_overrun");
            entry.next_pulse_tick = tick + u64::from(entry.period_ticks);
        }
    }
    // Clock.
    let timed = matches!(asset.lifetime, Lifetime::Timed(_));
    if timed && !(entry.suspended && asset.suspend.freeze_clock) {
        entry.remaining_ticks = entry.remaining_ticks.saturating_sub(1);
        if entry.remaining_ticks == 0 {
            match stacking::lapse(entry, asset.stack.as_ref()) {
                Lapse::End => {
                    end_effect(world, e, id, EndReason::Expired);
                    return;
                }
                Lapse::Emptied { old } => {
                    world.write_message(StacksChanged {
                        target: e,
                        id,
                        old,
                        new: 0,
                    });
                    end_effect(world, e, id, EndReason::Expired);
                    return;
                }
                Lapse::Popped { old, new } => {
                    world.write_message(StacksChanged {
                        target: e,
                        id,
                        old,
                        new,
                    });
                }
                Lapse::Renewed => {}
            }
        }
    }
    store(world, e, entry);
}

fn store(world: &mut World, e: Entity, entry: &AppliedEffect) {
    if let Some(mut list) = world.get_mut::<AppliedEffects>(e)
        && let Some(slot) = list.get_mut(entry.id)
    {
        slot.copy_from(entry);
    }
}

// ---------------------------------------------------------------------------------------------
// Recompute (AbilitySet::Recompute).

/// Fills `out` with the live modifiers of `e` in evaluation order, with their attribute; warnings
/// are added to `warnings`. Returns whether every one of them is fixed (`EffectEntry::fixed_mods`):
/// then `out` depends on nothing but `e`'s `AppliedEffects` and the assets.
fn collect_mods(
    world: &World,
    e: Entity,
    out: &mut Vec<(AttrId, LiveMod)>,
    warnings: &mut Vec<&'static str>,
) -> bool {
    out.clear();
    let Some(list) = world.get::<AppliedEffects>(e) else {
        return true;
    };
    // Borrowed once per entity, and each modifier's attribute id was hashed at insert: this runs
    // for every live effect every step (spec 53 § 10 budget).
    let Some(assets) = world.get_resource::<assets::AbilityAssets>() else {
        return true;
    };
    let mut fixed = true;
    for entry in list.list.iter().filter(|x| !x.suspended) {
        let Some(found) = assets.effect_entry(&entry.asset) else {
            continue;
        };
        fixed &= found.fixed_mods;
        let asset = &*found.asset;
        for ((i, m), &attribute) in asset
            .live_mods
            .iter()
            .enumerate()
            .zip(found.live_attributes.iter())
        {
            if let Some(q) = &m.when
                && !effective_matches(world, e, q)
            {
                continue;
            }
            let amount = if m.amount.is_live() {
                let ctx = Ctx {
                    level: entry.level,
                    stacks: entry.stacks,
                    source: entry.source,
                    target: e,
                    params: &entry.params,
                };
                magnitude(world, asset, &m.amount, ctx, warnings)
            } else {
                entry.live_amounts.get(i).copied().unwrap_or(0.0)
            };
            out.push((
                attribute,
                LiveMod {
                    effect: entry.id,
                    applied_tick: entry.applied_tick,
                    op: m.op,
                    amount,
                    stacks: entry.stacks,
                    per_stack: m.per_stack,
                },
            ));
        }
    }
    fixed
}

/// Recomputes derived bases and `current` of `attrs` from `mods`, using `scratch` for one
/// attribute's modifiers; returns the breakdown of `explain` when asked, and the number of modifier
/// evaluations.
fn compute(
    attrs: &mut Attributes,
    mods: &[(AttrId, LiveMod)],
    explain: Option<AttrId>,
    scratch: &mut Vec<LiveMod>,
) -> (Option<AttributeBreakdown>, u64) {
    attrs.ensure_order();
    let order = std::mem::take(&mut attrs.order);
    let mut breakdown = None;
    let mut evals = 0u64;
    for &index in &order {
        let index = usize::from(index);
        let id = attrs.list()[index].id;
        let derived = attrs.list()[index].derived.as_ref().map(|d| {
            let mut sum = 0.0f32;
            for (input, weight) in &d.inputs {
                sum += attrs.get(*input).unwrap_or(0.0) * weight;
            }
            d.offset + sum
        });
        if let Some(v) = derived {
            let a = &mut attrs.list_mut()[index];
            if v.is_finite() {
                a.base = v.clamp(a.min, a.max.max(a.min));
            }
        }
        scratch.clear();
        scratch.extend(mods.iter().filter(|(a, _)| *a == id).map(|(_, m)| *m));
        evals += scratch.len() as u64;
        let max = attrs.effective_max(index);
        let (base, min) = {
            let a = &attrs.list()[index];
            (a.base, a.min)
        };
        let current = if explain == Some(id) {
            let mut b = AttributeBreakdown {
                attribute: id,
                name: attrs.list()[index].name.clone(),
                base,
                phases: Vec::new(),
                replace_winner: None,
                unclamped: base,
                clamp: (min, max),
                current: base,
            };
            let c = evaluate(base, scratch, min, max, Some(&mut b));
            breakdown = Some(b);
            c
        } else {
            evaluate(base, scratch, min, max, None)
        };
        attrs.list_mut()[index].current = current;
    }
    attrs.order = order;
    (breakdown, evals)
}

/// How `e`'s attribute `a` is computed now: the base, each phase with its modifiers and the value
/// after it, the `Replace` winner and the clamp (§ 3.2).
#[must_use]
pub fn explain_attribute(world: &World, e: Entity, a: AttrId) -> Option<AttributeBreakdown> {
    let mut attrs = world.get::<Attributes>(e)?.clone();
    attrs.index_of(a)?;
    let mut mods = Vec::new();
    collect_mods(world, e, &mut mods, &mut Vec::new());
    compute(&mut attrs, &mods, Some(a), &mut Vec::new()).0
}

/// Buffers of the recompute pass, kept between steps.
#[derive(Default)]
pub(crate) struct RecomputeScratch {
    order: EntityOrder,
    mods: Vec<(AttrId, LiveMod)>,
    one_attribute: Vec<LiveMod>,
    warnings: Vec<&'static str>,
    live_keys: Vec<u64>,
}

/// Drops the chance-stream rows (and their cached generators) whose key matches no live entity's
/// `EntityGuid` low half (deep-zai finding 2): a streaming world with short-lived guid-bearing
/// targets would otherwise keep one row per target forever, in memory and in the save. A guid
/// that comes back after its rows were pruned starts a fresh stream (ADR-7573).
fn prune_rolls(world: &mut World, guid_query: &mut QueryState<&EntityGuid>, scratch: &mut RecomputeScratch) {
    if !world
        .get_resource::<AbilityCounters>()
        .is_some_and(|c| !c.rolls.is_empty())
    {
        return;
    }
    scratch.live_keys.clear();
    scratch
        .live_keys
        .extend(guid_query.iter(world).map(|guid| guid.0.0 as u64));
    scratch.live_keys.sort_unstable();
    scratch.live_keys.dedup();
    let live = &scratch.live_keys;
    if let Some(mut counters) = world.get_resource_mut::<AbilityCounters>() {
        counters
            .rolls
            .retain(|(key, _, _)| live.binary_search(key).is_ok());
    }
    if let Some(mut streams) = world.get_resource_mut::<RollStreams>() {
        streams.retain_live(live);
    }
}

/// Whether a full recompute of `e` would give what `e` holds (spec 53 § 10's dirty flag, for an
/// entity with live effects): its last recompute saw only fixed modifiers (`Attributes::fixed_mods`),
/// and since this pass last ran nothing wrote its `AppliedEffects` or its `Attributes`. Those, with
/// the assets (the caller checks them), are all a fixed modifier reads; the pass's own write of
/// `Attributes` is not seen as a change on its next run. A write of clocks alone (`move_clocks`)
/// is told from any other write by its own change tick (ADR-2012).
fn settled(world: &World, e: Entity) -> bool {
    let Ok(entity) = world.get_entity(e) else {
        return false;
    };
    let (Some(list), Some(attrs)) = (
        entity.get_change_ticks::<AppliedEffects>(),
        entity.get_ref::<Attributes>(),
    ) else {
        return false;
    };
    let (last_run, this_run) = (world.last_change_tick(), world.read_change_tick());
    // Unwritten since this pass last ran, or last written by `move_clocks` alone: that write's
    // tick is the list's, and the list was unwritten before it.
    let quiet = !list.is_changed(last_run, this_run)
        || (!list.is_added(last_run, this_run)
            && attrs.clock_write.is_some_and(|(before, written)| {
                list.changed == written && !before.is_newer_than(last_run, this_run)
            }));
    attrs.fixed_mods && quiet && !attrs.is_changed()
}

/// Whether something wrote `e`'s `AppliedEffects` since the recompute pass last ran.
fn effects_changed(world: &World, e: Entity) -> bool {
    world
        .get_entity(e)
        .ok()
        .and_then(|entity| entity.get_ref::<AppliedEffects>())
        .is_some_and(|list| list.is_changed())
}

/// `FixedUpdate`, `AbilitySet::Recompute`: derived values, modifiers, clamps (dirty-flag skip).
pub(crate) fn recompute_pass(
    world: &mut World,
    guid_query: &mut QueryState<&EntityGuid>,
    query: &mut QueryState<OrderItem, With<Attributes>>,
    mut scratch: Local<RecomputeScratch>,
) {
    let _span = tracing::info_span!(target: "regia_framework", "abilities_recompute").entered();
    let scratch = &mut *scratch;
    prune_rolls(world, guid_query, scratch);
    let mut evals = 0u64;
    let mut skipped = 0u64;
    let mut entities = 0u64;
    let mut live = 0u64;
    // A host's or a reload's write to the assets may change any modifier.
    let assets_changed = world.is_resource_changed::<assets::AbilityAssets>();
    let ordered = scratch.order.take(world, query);
    for &e in &ordered {
        entities += 1;
        let effects = world.get::<AppliedEffects>(e).map_or(0, |l| l.list.len());
        live += effects as u64;
        let clean = world.get::<Attributes>(e).is_some_and(|a| a.clean);
        // An emptied list is a change too: a host's `list.clear()` leaves `clean` set.
        if clean && effects == 0 && !effects_changed(world, e) {
            skipped += 1;
            continue;
        }
        if clean && !assets_changed && settled(world, e) {
            skipped += 1;
            continue;
        }
        let fixed = collect_mods(world, e, &mut scratch.mods, &mut scratch.warnings);
        for w in scratch.warnings.drain(..) {
            warn(world, w);
        }
        if let Some(mut attrs) = world.get_mut::<Attributes>(e) {
            let (_, n) = compute(&mut attrs, &scratch.mods, None, &mut scratch.one_attribute);
            attrs.clean = true;
            attrs.fixed_mods = fixed;
            evals += n;
        }
    }
    scratch.order.give_back(ordered);
    if let Some(mut stats) = world.get_resource_mut::<AbilityStats>() {
        stats.modifier_evaluations = evals;
        stats.entities_skipped = skipped;
        stats.entities = entities;
        stats.live_effects = live;
    }
}

// ---------------------------------------------------------------------------------------------
// Notify (AbilitySet::Notify).

/// Buffers of the notify pass, kept between steps.
#[derive(Default)]
pub(crate) struct NotifyScratch {
    attribute_order: EntityOrder,
    tag_order: EntityOrder,
    now: Vec<ReportRow>,
    /// Recent `AttributeChanged` snapshots, shared by the entities that report alike (ADR-2426).
    reports: Vec<Arc<[ReportRow]>>,
    changes: Vec<AttributeChanged>,
    tags: Vec<Tag>,
}

/// `FixedUpdate`, `AbilitySet::Notify`: `AttributeChanged` and `EffectiveTagsChanged`.
pub(crate) fn notify_pass(
    world: &mut World,
    attribute_query: &mut QueryState<OrderItem, With<Attributes>>,
    tag_query: &mut QueryState<OrderItem, With<TagCounts>>,
    mut scratch: Local<NotifyScratch>,
) {
    let _span = tracing::info_span!(target: "regia_framework", "abilities_notify").entered();
    let scratch = &mut *scratch;
    let ordered = scratch.attribute_order.take(world, attribute_query);
    for &e in &ordered {
        if world.get::<Attributes>(e).is_none_or(Attributes::matches_report) {
            continue;
        }
        let Some(mut attrs) = world.get_mut::<Attributes>(e) else {
            continue;
        };
        scratch.now.clear();
        scratch
            .now
            .extend(attrs.list().iter().map(|a| (a.id, a.base, a.current)));
        let source = attrs.last_source;
        // Diffed by attribute id: an attribute added this step (or every attribute after a load,
        // when the snapshot is empty) only takes its baseline, and the others still report
        // (a count change used to drop every change of the step; ADR-1809).
        for &(id, nb, nc) in &scratch.now {
            let Some((ob, oc)) = attrs.reported.find(id) else {
                continue;
            };
            if differs(ob, nb) {
                scratch.changes.push(AttributeChanged {
                    entity: e,
                    attribute: id,
                    lane: AttrLane::Base,
                    old: ob,
                    new: nb,
                    source,
                });
            }
            if differs(oc, nc) {
                scratch.changes.push(AttributeChanged {
                    entity: e,
                    attribute: id,
                    lane: AttrLane::Current,
                    old: oc,
                    new: nc,
                    source,
                });
            }
        }
        attrs.reported.store(&scratch.now, &mut scratch.reports);
    }
    scratch.attribute_order.give_back(ordered);
    for c in scratch.changes.drain(..) {
        world.write_message(c);
    }
    let mut tag_changes = Vec::new();
    let ordered = scratch.tag_order.take(world, tag_query);
    for &e in &ordered {
        effective_tags_into(world, e, &mut scratch.tags);
        if world
            .get::<TagCounts>(e)
            .is_none_or(|counts| counts.reported.as_ref() == Some(&scratch.tags))
        {
            continue;
        }
        let Some(mut counts) = world.get_mut::<TagCounts>(e) else {
            continue;
        };
        let Some(before) = counts.reported.take() else {
            counts.reported = Some(scratch.tags.clone());
            continue;
        };
        let added: Vec<Tag> = scratch
            .tags
            .iter()
            .filter(|t| !before.contains(t))
            .copied()
            .collect();
        let removed: Vec<Tag> = before
            .iter()
            .filter(|t| !scratch.tags.contains(t))
            .copied()
            .collect();
        counts.reported = Some(scratch.tags.clone());
        if !added.is_empty() || !removed.is_empty() {
            tag_changes.push(EffectiveTagsChanged {
                entity: e,
                added,
                removed,
            });
        }
    }
    scratch.tag_order.give_back(ordered);
    for c in tag_changes {
        world.write_message(c);
    }
}

fn differs(old: f32, new: f32) -> bool {
    (new - old).abs() > f32::EPSILON * old.abs().max(1.0)
}

#[cfg(test)]
mod roll_tests {
    use super::*;
    use regia_core::id::Guid;

    fn world_with(guid: u128) -> (World, Entity) {
        let mut world = World::new();
        world.insert_resource(AbilitySettings::default());
        world.insert_resource(AbilityCounters::default());
        world.insert_resource(RollStreams::default());
        let e = world.spawn(EntityGuid(Guid(guid))).id();
        (world, e)
    }

    fn replay_value(seed: u64, key: u64, asset: u64, position: u64) -> f32 {
        let mut rng = regia_core::math::Rng::with_stream(seed ^ key, asset);
        for _ in 0..position {
            rng.next_u32();
        }
        rng.f32()
    }

    /// The cached advance draws exactly the values the position replay does, in order (§ 7).
    #[test]
    fn cached_streams_draw_the_replays_values() {
        let (mut world, e) = world_with(7);
        for position in 0..16u64 {
            let got = roll(&mut world, e, "coin").expect("a guid target rolls");
            let want = replay_value(
                AbilitySettings::default().seed,
                7,
                crate::tags::fnv1a64(b"coin"),
                position,
            );
            assert_eq!(got.to_bits(), want.to_bits(), "position {position}");
        }
    }

    /// A stale entry (a load replaced the counters, or a host edited them) is rejected and
    /// rebuilt by the replay, never trusted.
    #[test]
    fn a_stale_stream_entry_falls_back_to_the_replay() {
        let (mut world, e) = world_with(9);
        for _ in 0..3 {
            let _ = roll(&mut world, e, "coin");
        }
        let mut loaded = AbilityCounters::default();
        loaded.rolls.push((9, crate::tags::fnv1a64(b"coin"), 1));
        world.insert_resource(loaded);
        let got = roll(&mut world, e, "coin").expect("rolls");
        let want = replay_value(
            AbilitySettings::default().seed,
            9,
            crate::tags::fnv1a64(b"coin"),
            1,
        );
        assert_eq!(got.to_bits(), want.to_bits(), "the stale entry was not trusted");
        let got = roll(&mut world, e, "coin").expect("rolls");
        let want = replay_value(
            AbilitySettings::default().seed,
            9,
            crate::tags::fnv1a64(b"coin"),
            2,
        );
        assert_eq!(got.to_bits(), want.to_bits(), "the stream continues from there");
    }
}
