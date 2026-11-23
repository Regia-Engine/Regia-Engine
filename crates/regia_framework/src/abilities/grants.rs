// SPDX-License-Identifier: MPL-2.0
//! Abilities (`docs/specs/53-abilities-attributes-effects.md` § 2.6): authored actions, grants,
//! the state machine, activation checks, cost and cooldown, triggers, input and picks.

use std::cell::Cell;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::ecs::change_detection::DetectChanges;
use regia_ecs::ecs::message::{MessageCursor, Messages};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use super::cues::{self, CueAt, CueBase, CueBinding, CueKind};
use super::effects::{
    self, ApplyEffect, Ctx, Edit, EffectAsset, EffectiveTags, PlannedBases, count_tags, effective_container,
    effective_tags_into, magnitude, plan_edits,
};
use super::modifiers::RunStateTypes;
use super::{
    AbilityCounters, AbilityError, AbilitySettings, AbilityStats, AttrId, Attributes, Cheats, EntityOrder,
    GrantId, IntentSeq, OrderItem, assets,
};
use crate::tags::{Tag, TagContainer, TagLookup, TagQuery};

/// Most grants one [`AbilityBook`] holds; a further `grant_ability` fails with `grant_limit`
/// (§ 10).
pub const MAX_GRANTS_PER_BOOK: usize = 32;

/// Most messages of one request kind a step takes (§ 10); the rest of a burst is dropped with a
/// `message_budget` warning.
pub const MAX_MESSAGES_PER_STEP: usize = 4096;

/// When cost and cooldown are paid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ChargeMode {
    /// When the run starts.
    #[default]
    OnStart,
    /// When the driver calls `charge`.
    Explicit,
}

/// What run data a grant keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RunStateMode {
    /// Only `Grant::scratch`.
    #[default]
    ScratchOnly,
    /// One value of `run_type` per grant, kept between runs.
    KeepAcrossRuns,
    /// A fresh default `run_type` value per run, dropped at the end.
    FreshEachRun,
}

/// What a start request does while the grant runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum WhenRunning {
    /// Refuse (`already_running`).
    #[default]
    Reject,
    /// End the run and start again.
    Restart,
    /// Start once more when the run ends.
    QueueOne,
}

/// Where an ability runs once networking lands (parsed and stored; everything runs as
/// `HostOnly` until then, § 2.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RunSite {
    /// The owning client only.
    OwnerOnly,
    /// The owner predicts, the host confirms.
    OwnerPredicts,
    /// The host starts it.
    HostStarts,
    /// The host only.
    #[default]
    HostOnly,
}

/// What starts an ability besides a direct request.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Trigger {
    /// A matching `AbilitySignal` (hierarchy match: a child tag starts a parent trigger).
    Signal(Tag),
    /// The owner gaining a tag.
    TagGained(Tag),
    /// Run while the owner holds the tag; end when it is lost.
    WhileTagged(Tag),
}

fn yes() -> bool {
    true
}

/// An authored ability (`.ability.ron`, § 4.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbilityAsset {
    /// Format version (1).
    pub format_version: u32,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// What it is for.
    #[serde(default)]
    pub description: String,
    /// Tags describing the ability.
    #[serde(default)]
    pub kind_tags: Vec<Tag>,
    /// Tags counted on the owner while it runs.
    #[serde(default)]
    pub run_tags: Vec<Tag>,
    /// Running abilities whose `kind_tags` match are cancelled when this starts.
    #[serde(default)]
    pub cancels: Option<TagQuery>,
    /// While this runs, abilities whose `kind_tags` match cannot start.
    #[serde(default)]
    pub blocks: Option<TagQuery>,
    /// The owner must match.
    #[serde(default)]
    pub owner_needs: Option<TagQuery>,
    /// The owner must not match.
    #[serde(default)]
    pub owner_forbids: Option<TagQuery>,
    /// Each picked entity must match.
    #[serde(default)]
    pub target_needs: Option<TagQuery>,
    /// Each picked entity must not match.
    #[serde(default)]
    pub target_forbids: Option<TagQuery>,
    /// An instant effect paid when charging.
    #[serde(default)]
    pub cost: Option<String>,
    /// A timed effect with status tags (the cooldown tags), applied when charging.
    #[serde(default)]
    pub cooldown: Option<String>,
    /// When to pay.
    #[serde(default)]
    pub charge: ChargeMode,
    /// Run data kept.
    #[serde(default)]
    pub run_state: RunStateMode,
    /// The registered run-state type name for `KeepAcrossRuns`/`FreshEachRun`.
    #[serde(default)]
    pub run_type: String,
    /// What a start request does while running.
    #[serde(default)]
    pub when_running: WhenRunning,
    /// Triggers.
    #[serde(default)]
    pub triggers: Vec<Trigger>,
    /// Highest level (0 = no limit).
    #[serde(default)]
    pub max_level: u32,
    /// Whether `cancel` works.
    #[serde(default = "yes")]
    pub cancelable: bool,
    /// Forces `end` after this many ticks.
    #[serde(default)]
    pub max_run_ticks: Option<u32>,
    /// The run must receive picks before charging.
    #[serde(default)]
    pub needs_picks: bool,
    /// Ends a run still awaiting picks after this many ticks (`pick_timeout`).
    #[serde(default)]
    pub pick_timeout_ticks: Option<u32>,
    /// Default input code of new grants.
    #[serde(default)]
    pub input_code: Option<u16>,
    /// Network run site (stored only).
    #[serde(default)]
    pub run_site: RunSite,
    /// Cue bindings (`Applied` = started, `Removed` = ended).
    #[serde(default)]
    pub cues: Vec<CueBinding>,
}

impl AbilityAsset {
    /// A minimal ability.
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            format_version: 1,
            name: name.to_owned(),
            description: String::new(),
            kind_tags: Vec::new(),
            run_tags: Vec::new(),
            cancels: None,
            blocks: None,
            owner_needs: None,
            owner_forbids: None,
            target_needs: None,
            target_forbids: None,
            cost: None,
            cooldown: None,
            charge: ChargeMode::OnStart,
            run_state: RunStateMode::ScratchOnly,
            run_type: String::new(),
            when_running: WhenRunning::Reject,
            triggers: Vec::new(),
            max_level: 0,
            cancelable: true,
            max_run_ticks: None,
            needs_picks: false,
            pick_timeout_ticks: None,
            input_code: None,
            run_site: RunSite::HostOnly,
            cues: Vec::new(),
        }
    }

    /// The kind tags as a container.
    #[must_use]
    pub fn kind_container(&self) -> TagContainer {
        self.kind_tags.iter().copied().collect()
    }
}

/// The state of a grant. `Cooling` is never stored: a `Ready` grant whose owner holds a cooldown
/// tag reads as `Cooling`.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AbilityState {
    /// Can start.
    #[default]
    Ready,
    /// Running.
    Running,
    /// Ends at the next step boundary.
    Ending,
    /// Ready, but a cooldown tag is held (derived).
    Cooling,
}

impl AbilityState {
    /// Lowercase name.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            AbilityState::Ready => "ready",
            AbilityState::Running => "running",
            AbilityState::Ending => "ending",
            AbilityState::Cooling => "cooling",
        }
    }
}

/// One pick.
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
pub enum Pick {
    /// An entity.
    Entity(Entity),
    /// A point.
    Point(Vec3),
    /// A hit.
    Hit {
        /// The entity hit.
        entity: Entity,
        /// Where.
        point: Vec3,
        /// Surface normal.
        normal: Vec3,
    },
    /// A direction.
    Direction(Vec3),
}

/// Most picks one `Picks` holds.
pub const MAX_PICKS: usize = 32;

/// Up to 32 picks, in producer order.
#[derive(Reflect, Debug, Clone, PartialEq, Default)]
pub struct Picks {
    /// The picks.
    pub list: Vec<Pick>,
}

impl Picks {
    /// Picks from a list (truncated to 32).
    #[must_use]
    pub fn new(mut list: Vec<Pick>) -> Self {
        list.truncate(MAX_PICKS);
        Self { list }
    }

    /// The picked entities.
    pub fn entities(&self) -> impl Iterator<Item = Entity> + '_ {
        self.list.iter().filter_map(|p| match p {
            Pick::Entity(e) | Pick::Hit { entity: e, .. } => Some(*e),
            _ => None,
        })
    }
}

/// A payload that can trigger abilities.
#[derive(Message, Debug, Clone, PartialEq)]
pub struct AbilitySignal {
    /// Whose grants are matched.
    pub target: Entity,
    /// The signal tag.
    pub tag: Tag,
    /// Who sent it.
    pub source: Option<Entity>,
    /// A number.
    pub amount: f32,
    /// Picks.
    pub picks: Picks,
    /// Extra context tags.
    pub context: TagContainer,
}

impl AbilitySignal {
    /// A bare signal.
    #[must_use]
    pub fn new(target: Entity, tag: Tag) -> Self {
        Self {
            target,
            tag,
            source: None,
            amount: 0.0,
            picks: Picks::default(),
            context: TagContainer::default(),
        }
    }
}

/// Data of the current run.
#[derive(Reflect, Debug, Clone, PartialEq, Default)]
pub struct RunInfo {
    /// Tick the run started.
    pub started_tick: u64,
    /// Cost and cooldown paid.
    pub charged: bool,
    /// Waiting for `submit_picks`.
    pub awaiting_picks: bool,
    /// Picks (not saved, § 4.6).
    #[reflect(skip_serializing)]
    pub picks: Picks,
    /// The triggering signal's tag.
    pub signal: Option<Tag>,
    /// The triggering signal's amount.
    pub signal_amount: f32,
    /// The `WhileTagged` tag that started it.
    pub while_tag: Option<Tag>,
    /// Client intent.
    pub intent: IntentSeq,
}

/// Step fields copied off the ability asset so the trigger and state passes do not clone the asset
/// handle every fixed step. Refreshed when [`super::assets::AbilityAssets`] publishes a new
/// generation. Not saved and not part of [`crate::abilities::abilities_state_hash`].
#[derive(Clone, Debug)]
pub(crate) struct GrantStepData {
    triggers: Vec<Trigger>,
    max_run_ticks: Option<u32>,
    pick_timeout_ticks: Option<u32>,
    asset_gen: u64,
}

/// A `WhileTagged` refusal that can be repeated while its inputs are unchanged.
///
/// Besides the cause-specific inputs it records the asset generation and the cheat switches: an
/// ability or effect reload, or a cheat toggle, can turn the refusal into a start (corr-framework-a
/// F1, ADR-5572).
#[derive(Clone, Copy, Debug)]
pub(crate) struct WhileHold {
    cause: RefusalCause,
    tags_fp: u64,
    attr_tick: u32,
    block_fp: u64,
    asset_gen: u64,
    cheats: Cheats,
    level: u32,
}

/// One grant of an ability.
#[derive(Reflect, Debug, Clone)]
pub struct Grant {
    /// Id (never reused while the book lives).
    pub id: GrantId,
    /// Ability asset path.
    pub asset: String,
    /// Level.
    pub level: u32,
    /// Bound input code.
    pub input_code: Option<u16>,
    /// Stored state (never `Cooling`).
    pub state: AbilityState,
    /// The current run.
    pub run: Option<RunInfo>,
    /// Scratch numbers, kept across runs.
    pub scratch: [f32; 4],
    /// The run-state value as RON text.
    pub run_value: Option<String>,
    /// Removed when its run ends.
    pub pending_removal: bool,
    /// Removed after its first run ends.
    pub one_shot: bool,
    /// A start request arrived while running (`QueueOne`).
    pub queued: bool,
    /// Triggers added by the grant.
    pub extra_triggers: Vec<Trigger>,
    /// Cached triggers and run limits. Ignored by reflect, so a save/load leaves it empty and the
    /// next pass refills it.
    #[reflect(ignore)]
    pub(crate) step_cache: Option<GrantStepData>,
    /// The last cacheable `WhileTagged` refusal. Cleared when the trigger tag is lost or a start
    /// succeeds.
    #[reflect(ignore)]
    pub(crate) while_hold: Option<WhileHold>,
}

impl PartialEq for Grant {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.asset == other.asset
            && self.level == other.level
            && self.input_code == other.input_code
            && self.state == other.state
            && self.run == other.run
            && self.scratch == other.scratch
            && self.run_value == other.run_value
            && self.pending_removal == other.pending_removal
            && self.one_shot == other.one_shot
            && self.queued == other.queued
            && self.extra_triggers == other.extra_triggers
    }
}

/// The grants of one entity, sorted by [`GrantId`].
#[derive(Component, Reflect, Debug, Clone, Default)]
#[reflect(Component, Default)]
pub struct AbilityBook {
    /// Grants, ascending id.
    pub grants: Vec<Grant>,
    /// Last id handed out.
    pub last_id: u32,
    /// A press starts only the first grant that starts.
    pub one_per_press: bool,
    /// Blocked input codes.
    pub blocked_inputs: Vec<u16>,
    /// Effective tags at the last trigger pass.
    #[reflect(ignore)]
    pub(crate) seen_tags: Option<Vec<Tag>>,
}

impl AbilityBook {
    /// The grant `id`.
    #[must_use]
    pub fn get(&self, id: GrantId) -> Option<&Grant> {
        self.grants.iter().find(|g| g.id == id)
    }

    fn get_mut(&mut self, id: GrantId) -> Option<&mut Grant> {
        self.grants.iter_mut().find(|g| g.id == id)
    }
}

/// Marker: the entity's abilities cannot start (`owner_locked`).
#[derive(Component, Reflect, Debug, Clone, Copy, Default)]
#[reflect(Component, Default)]
pub struct AbilitiesLocked;

/// Options of [`grant_ability`].
#[derive(Debug, Clone, PartialEq)]
pub struct GrantOptions {
    /// Level (default 1).
    pub level: u32,
    /// Input code (default: the asset's).
    pub input_code: Option<u16>,
    /// Start at once; keep nothing when the check fails.
    pub try_now: bool,
    /// Remove after the first run ends.
    pub one_shot: bool,
    /// Extra triggers.
    pub extra_triggers: Vec<Trigger>,
}

impl Default for GrantOptions {
    fn default() -> Self {
        Self {
            level: 1,
            input_code: None,
            try_now: false,
            one_shot: false,
            extra_triggers: Vec::new(),
        }
    }
}

/// Context of a start.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StartContext {
    /// Picks.
    pub picks: Option<Picks>,
    /// The triggering signal.
    pub signal: Option<AbilitySignal>,
    /// Client intent.
    pub intent: IntentSeq,
}

/// Why a start (or a charge) was refused (§ 2.6 check order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefusalCause {
    /// No such grant.
    NotGranted,
    /// The owner has `AbilitiesLocked`.
    OwnerLocked,
    /// The grant's input code is blocked.
    InputBlocked,
    /// The grant's level is 0.
    LevelZero,
    /// Already running (`when_running: Reject`).
    AlreadyRunning,
    /// `owner_needs` failed.
    OwnerMissingTag,
    /// `owner_forbids` matched.
    OwnerForbiddenTag,
    /// A running ability's `blocks` matched.
    BlockedByAbility,
    /// A cooldown tag is held.
    OnCooldown,
    /// The cost would take an attribute below its minimum.
    CannotAfford,
    /// A pick failed the target requirements.
    TargetRejected,
    /// A run awaiting picks timed out.
    PickTimeout,
}

impl RefusalCause {
    /// Lowercase code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            RefusalCause::NotGranted => "not_granted",
            RefusalCause::OwnerLocked => "owner_locked",
            RefusalCause::InputBlocked => "input_blocked",
            RefusalCause::LevelZero => "level_zero",
            RefusalCause::AlreadyRunning => "already_running",
            RefusalCause::OwnerMissingTag => "owner_missing_tag",
            RefusalCause::OwnerForbiddenTag => "owner_forbidden_tag",
            RefusalCause::BlockedByAbility => "blocked_by_ability",
            RefusalCause::OnCooldown => "on_cooldown",
            RefusalCause::CannotAfford => "cannot_afford",
            RefusalCause::TargetRejected => "target_rejected",
            RefusalCause::PickTimeout => "pick_timeout",
        }
    }
}

/// A refused start, with the blocking tag or the short attribute where there is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
    /// Why.
    pub cause: RefusalCause,
    /// The blocking tag.
    pub tag: Option<Tag>,
    /// The short attribute (`cannot_afford`).
    pub attribute: Option<AttrId>,
}

impl Refusal {
    fn new(cause: RefusalCause) -> Self {
        Self {
            cause,
            tag: None,
            attribute: None,
        }
    }

    fn tag(cause: RefusalCause, tag: Option<Tag>) -> Self {
        Self {
            cause,
            tag,
            attribute: None,
        }
    }
}

/// A lifecycle moment of a grant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LifecycleEvent {
    /// The run started.
    Started,
    /// A start (or charge) was refused.
    Refused(Refusal),
    /// Cost and cooldown were paid.
    Charged,
    /// The run was cancelled.
    Cancelled,
    /// The run is ending.
    Ending {
        /// Whether by cancel.
        cancelled: bool,
    },
    /// The run ended; the grant is ready (or cooling).
    Ended,
    /// The grant was removed.
    Revoked,
    /// The bound input was released while running.
    InputReleased,
    /// Picks were submitted.
    PicksSubmitted,
}

impl LifecycleEvent {
    /// Lowercase name.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            LifecycleEvent::Started => "started",
            LifecycleEvent::Refused(_) => "refused",
            LifecycleEvent::Charged => "charged",
            LifecycleEvent::Cancelled => "cancelled",
            LifecycleEvent::Ending { .. } => "ending",
            LifecycleEvent::Ended => "ended",
            LifecycleEvent::Revoked => "revoked",
            LifecycleEvent::InputReleased => "input_released",
            LifecycleEvent::PicksSubmitted => "picks_submitted",
        }
    }
}

/// Written for every grant lifecycle moment, in generation order.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct AbilityLifecycle {
    /// The owner.
    pub owner: Entity,
    /// The grant.
    pub grant: GrantId,
    /// What happened.
    pub event: LifecycleEvent,
}

/// Request: start a grant (consumed in `AbilitySet::Requests`).
#[derive(Message, Debug, Clone, PartialEq)]
pub struct StartAbility {
    /// The owner.
    pub owner: Entity,
    /// The grant.
    pub grant: GrantId,
    /// Start context.
    pub context: StartContext,
}

/// Request: cancel a grant's run.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct CancelAbility {
    /// The owner.
    pub owner: Entity,
    /// The grant.
    pub grant: GrantId,
}

/// Request: revoke a grant.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevokeAbility {
    /// The owner.
    pub owner: Entity,
    /// The grant.
    pub grant: GrantId,
    /// Remove when the run ends.
    pub defer: bool,
}

/// An input press or release for a code (consumed in `AbilitySet::Input`).
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbilityInput {
    /// The owner.
    pub entity: Entity,
    /// The input code.
    pub code: u16,
    /// Pressed (true) or released.
    pub down: bool,
}

fn lifecycle(world: &mut World, owner: Entity, grant: GrantId, event: LifecycleEvent) {
    world.write_message(AbilityLifecycle { owner, grant, event });
}

fn tick(world: &World) -> u64 {
    world.get_resource::<AbilityCounters>().map_or(0, |c| c.tick)
}

/// The cooldown tags of an ability (its cooldown effect's status tags).
#[must_use]
pub fn cooldown_tags(world: &World, ability: &AbilityAsset) -> Vec<Tag> {
    ability
        .cooldown
        .as_deref()
        .and_then(|p| assets::effect(world, p))
        .map(|e| e.status_tags.clone())
        .unwrap_or_default()
}

/// The state of a grant, with `Cooling` derived from the cooldown tags.
#[must_use]
pub fn ability_state(world: &World, owner: Entity, g: GrantId) -> Option<AbilityState> {
    let grant = world.get::<AbilityBook>(owner)?.get(g)?;
    if grant.state == AbilityState::Ready
        && let Some(asset) = assets::ability(world, &grant.asset)
        && cooldown_tags(world, &asset)
            .iter()
            .any(|t| effects::effective_has(world, owner, *t))
    {
        return Some(AbilityState::Cooling);
    }
    Some(grant.state)
}

/// Grants `ability` (an asset path) to `owner`.
pub fn grant_ability(
    world: &mut World,
    owner: Entity,
    ability: &str,
    opts: GrantOptions,
) -> Result<GrantId, AbilityError> {
    let asset =
        assets::ability(world, ability).ok_or_else(|| AbilityError::AssetNotFound(ability.to_owned()))?;
    let asset_gen = world
        .get_resource::<assets::AbilityAssets>()
        .map(|assets| assets.generation())
        .unwrap_or(0);
    let step_cache = Some(GrantStepData {
        triggers: asset.triggers.clone(),
        max_run_ticks: asset.max_run_ticks,
        pick_timeout_ticks: asset.pick_timeout_ticks,
        asset_gen,
    });
    let mut entity = world
        .get_entity_mut(owner)
        .map_err(|_| AbilityError::EntityNotFound)?;
    if entity.get::<AbilityBook>().is_none() {
        entity.insert(AbilityBook::default());
    }
    let Some(mut book) = entity.get_mut::<AbilityBook>() else {
        return Err(AbilityError::EntityNotFound);
    };
    if book.grants.len() >= MAX_GRANTS_PER_BOOK {
        return Err(AbilityError::GrantLimit);
    }
    book.last_id += 1;
    let id = GrantId(book.last_id);
    let level = if asset.max_level > 0 {
        opts.level.min(asset.max_level)
    } else {
        opts.level
    };
    book.grants.push(Grant {
        id,
        asset: ability.to_owned(),
        level,
        input_code: opts.input_code.or(asset.input_code),
        state: AbilityState::Ready,
        run: None,
        scratch: [0.0; 4],
        run_value: None,
        pending_removal: false,
        one_shot: opts.one_shot,
        queued: false,
        extra_triggers: opts.extra_triggers,
        step_cache,
        while_hold: None,
    });
    if opts.try_now
        && let Err(refusal) = activate(world, owner, id, StartContext::default())
    {
        remove_grant(world, owner, id);
        return Err(AbilityError::StartRefused(refusal.cause));
    }
    Ok(id)
}

fn remove_grant(world: &mut World, owner: Entity, g: GrantId) {
    if let Some(mut book) = world.get_mut::<AbilityBook>(owner) {
        book.grants.retain(|x| x.id != g);
    }
}

/// Removes a grant; a running grant is cancelled first. With `defer`, a running grant loses its
/// input binding at once and is removed when its run ends.
pub fn revoke_ability(world: &mut World, owner: Entity, g: GrantId, defer: bool) -> Result<(), AbilityError> {
    let grant = world
        .get::<AbilityBook>(owner)
        .and_then(|b| b.get(g))
        .cloned()
        .ok_or(AbilityError::GrantNotFound(g.0))?;
    if grant.state != AbilityState::Ready {
        if defer {
            if let Some(mut book) = world.get_mut::<AbilityBook>(owner)
                && let Some(x) = book.get_mut(g)
            {
                x.pending_removal = true;
                x.input_code = None;
            }
            return Ok(());
        }
        if grant.state == AbilityState::Running {
            lifecycle(world, owner, g, LifecycleEvent::Cancelled);
        }
        // `restart: true` skips the one-shot removal and the queue: the grant is removed just below.
        finish_run(world, owner, g, true);
    }
    remove_grant(world, owner, g);
    lifecycle(world, owner, g, LifecycleEvent::Revoked);
    Ok(())
}

/// An ability's `kind_tags` as a [`TagLookup`] (the block check runs every step for a held trigger).
struct KindTags<'a>(&'a [Tag]);

impl TagLookup for KindTags<'_> {
    fn has_tag(&self, tag: Tag) -> bool {
        self.0.iter().any(|t| t.matches(tag))
    }

    fn has_tag_exact(&self, tag: Tag) -> bool {
        self.0.contains(&tag)
    }
}

fn query_tag(world: &World, owner: Entity, q: &TagQuery, want_held: bool) -> Option<Tag> {
    q.tags_small()
        .into_iter()
        .find(|t| effects::effective_has(world, owner, *t) == want_held)
}

thread_local! {
    static ACTIVATION_CHECK_COUNT: Cell<u64> = const { Cell::new(0) };
}

/// How many times [`check_activation`] has run on this thread.
///
/// A `WhileTagged` grant that is already refused for a reason that has not changed (a cooldown
/// tag still held, a missing tag still missing) does not move this on later fixed steps.
#[must_use]
pub fn activation_check_count() -> u64 {
    ACTIVATION_CHECK_COUNT.with(|count| count.get())
}

/// Checks whether a start would succeed, in the § 2.6 order; the first failure is reported.
pub fn check_activation(world: &World, owner: Entity, g: GrantId, ctx: &StartContext) -> Result<(), Refusal> {
    ACTIVATION_CHECK_COUNT.with(|count| count.set(count.get().saturating_add(1)));
    let Some(book) = world.get::<AbilityBook>(owner) else {
        return Err(Refusal::new(RefusalCause::NotGranted));
    };
    let Some(grant) = book.get(g).filter(|x| !x.pending_removal) else {
        return Err(Refusal::new(RefusalCause::NotGranted));
    };
    let Some(asset) = assets::ability(world, &grant.asset) else {
        return Err(Refusal::new(RefusalCause::NotGranted));
    };
    if world.get::<AbilitiesLocked>(owner).is_some() {
        return Err(Refusal::new(RefusalCause::OwnerLocked));
    }
    if grant.input_code.is_some_and(|c| book.blocked_inputs.contains(&c)) {
        return Err(Refusal::new(RefusalCause::InputBlocked));
    }
    if grant.level == 0 {
        return Err(Refusal::new(RefusalCause::LevelZero));
    }
    if grant.state != AbilityState::Ready && asset.when_running == WhenRunning::Reject {
        return Err(Refusal::new(RefusalCause::AlreadyRunning));
    }
    let effective = EffectiveTags::of(world, owner);
    if let Some(q) = &asset.owner_needs
        && !q.eval_on(&effective)
    {
        return Err(Refusal::tag(
            RefusalCause::OwnerMissingTag,
            query_tag(world, owner, q, false),
        ));
    }
    if let Some(q) = &asset.owner_forbids
        && q.eval_on(&effective)
    {
        return Err(Refusal::tag(
            RefusalCause::OwnerForbiddenTag,
            query_tag(world, owner, q, true),
        ));
    }
    let kind = KindTags(&asset.kind_tags);
    for other in book
        .grants
        .iter()
        .filter(|x| x.id != g && matches!(x.state, AbilityState::Running | AbilityState::Ending))
    {
        if let Some(other_asset) = assets::ability(world, &other.asset)
            && let Some(q) = &other_asset.blocks
            && q.eval_on(&kind)
        {
            let tag = q.tags_small().into_iter().find(|t| kind.has_tag(*t));
            return Err(Refusal::tag(RefusalCause::BlockedByAbility, tag));
        }
    }
    let cheats = world
        .get_resource::<AbilitySettings>()
        .map(|s| s.cheats)
        .unwrap_or_default();
    if !cheats.no_cooldowns
        && let Some(cooldown) = asset.cooldown.as_deref().and_then(|p| assets::effect(world, p))
        && let Some(tag) = cooldown
            .status_tags
            .iter()
            .copied()
            .find(|t| effects::effective_has(world, owner, *t))
    {
        return Err(Refusal::tag(RefusalCause::OnCooldown, Some(tag)));
    }
    if !cheats.free_costs
        && let Err(attribute) = can_afford(world, owner, &asset, grant.level)
    {
        return Err(Refusal {
            cause: RefusalCause::CannotAfford,
            tag: None,
            attribute: Some(attribute),
        });
    }
    if let Some(picks) = &ctx.picks {
        check_targets(world, &asset, picks)?;
    }
    Ok(())
}

fn check_targets(world: &World, asset: &AbilityAsset, picks: &Picks) -> Result<(), Refusal> {
    for e in picks.entities() {
        let tags = effective_container(world, e);
        if let Some(q) = &asset.target_needs
            && !tags.matches(q)
        {
            return Err(Refusal::tag(
                RefusalCause::TargetRejected,
                q.tags().first().copied(),
            ));
        }
        if let Some(q) = &asset.target_forbids
            && tags.matches(q)
        {
            let tag = q.tags().into_iter().find(|t| tags.has(*t));
            return Err(Refusal::tag(RefusalCause::TargetRejected, tag));
        }
    }
    Ok(())
}

/// Dry run of the cost through the edit gates: `Err(attribute)` when it would go below `min`.
fn can_afford(world: &World, owner: Entity, ability: &AbilityAsset, level: u32) -> Result<(), AttrId> {
    let Some(cost) = ability.cost.as_deref().and_then(|p| assets::effect(world, p)) else {
        return Ok(());
    };
    let amounts = cost_amounts(world, &cost, owner, level);
    let edit = Edit {
        target: owner,
        source: Some(owner),
        effect: None,
        mods: &cost.base_edits,
        amounts: &amounts,
        stacks: 1,
        dry: true,
    };
    plan_edits(world, edit, &mut PlannedBases::new()).0
}

fn cost_amounts(world: &World, cost: &EffectAsset, owner: Entity, level: u32) -> SmallVec<[f32; 16]> {
    let mut warnings = Vec::new();
    let ctx = Ctx {
        level,
        stacks: 1,
        source: Some(owner),
        target: owner,
        params: &[],
    };
    cost.base_edits
        .iter()
        .map(|m| magnitude(world, cost, &m.amount, ctx, &mut warnings))
        .collect()
}

/// Starts a grant: checks, then cancels matching runs, counts run tags, charges if `OnStart`.
pub fn activate(world: &mut World, owner: Entity, g: GrantId, ctx: StartContext) -> Result<(), Refusal> {
    if let Err(refusal) = check_activation(world, owner, g, &ctx) {
        lifecycle(world, owner, g, LifecycleEvent::Refused(refusal));
        return Err(refusal);
    }
    start_run(world, owner, g, ctx)
}

/// Starts (or queues) a run. Only a refused `OnStart` charge is an error; the run is then ended.
fn start_run(world: &mut World, owner: Entity, g: GrantId, ctx: StartContext) -> Result<(), Refusal> {
    let Some(grant) = world.get::<AbilityBook>(owner).and_then(|b| b.get(g)).cloned() else {
        return Ok(());
    };
    let Some(asset) = assets::ability(world, &grant.asset) else {
        return Ok(());
    };
    if grant.state != AbilityState::Ready {
        match asset.when_running {
            WhenRunning::QueueOne => {
                if let Some(mut book) = world.get_mut::<AbilityBook>(owner)
                    && let Some(x) = book.get_mut(g)
                {
                    x.queued = true;
                }
                return Ok(());
            }
            WhenRunning::Restart => {
                if grant.state == AbilityState::Running {
                    lifecycle(world, owner, g, LifecycleEvent::Ending { cancelled: false });
                }
                finish_run(world, owner, g, true);
            }
            WhenRunning::Reject => return Ok(()),
        }
    }
    if let Some(q) = &asset.cancels {
        let others: Vec<GrantId> = world
            .get::<AbilityBook>(owner)
            .map(|b| {
                b.grants
                    .iter()
                    .filter(|x| x.id != g && x.state == AbilityState::Running)
                    .filter(|x| {
                        assets::ability(world, &x.asset).is_some_and(|a| a.kind_container().matches(q))
                    })
                    .map(|x| x.id)
                    .collect()
            })
            .unwrap_or_default();
        for other in others {
            let _ = cancel(world, owner, other);
        }
    }
    count_tags(world, owner, &asset.run_tags, true);
    let tick = tick(world);
    let default_value = match asset.run_state {
        RunStateMode::ScratchOnly => None,
        _ => world
            .get_resource::<RunStateTypes>()
            .and_then(|t| t.default_text(&asset.run_type)),
    };
    let awaiting = asset.needs_picks && ctx.picks.is_none();
    let (signal, signal_amount) = ctx
        .signal
        .as_ref()
        .map_or((None, 0.0), |s| (Some(s.tag), s.amount));
    let picks = ctx
        .picks
        .clone()
        .or_else(|| ctx.signal.as_ref().map(|s| s.picks.clone()))
        .unwrap_or_default();
    if let Some(mut book) = world.get_mut::<AbilityBook>(owner)
        && let Some(x) = book.get_mut(g)
    {
        x.state = AbilityState::Running;
        x.run = Some(RunInfo {
            started_tick: tick,
            charged: false,
            awaiting_picks: awaiting,
            picks,
            signal,
            signal_amount,
            while_tag: None,
            intent: ctx.intent,
        });
        match asset.run_state {
            RunStateMode::ScratchOnly => x.run_value = None,
            RunStateMode::FreshEachRun => x.run_value = default_value,
            RunStateMode::KeepAcrossRuns => {
                if x.run_value.is_none() {
                    x.run_value = default_value;
                }
            }
        }
    }
    lifecycle(world, owner, g, LifecycleEvent::Started);
    cues::fire_cues(
        world,
        &asset.cues,
        CueAt::Applied,
        Some(CueKind::Begin),
        CueBase {
            entity: owner,
            source: Some(owner),
            amount: 1.0,
            effect: None,
        },
    );
    if asset.charge == ChargeMode::OnStart
        && !awaiting
        && let Err(refusal) = pay(world, owner, g, &asset)
    {
        end_unpaid(world, owner, g);
        return Err(refusal);
    }
    Ok(())
}

/// Applies the cost, then the cooldown, and marks the run charged. The cost must take hold
/// (`applied` or `stacked`): a cost the activation dry-run could not foresee refusing (its `chance`,
/// `apply_if`, an immunity, the effect cap) leaves the run uncharged, applies no cooldown and is a
/// `cannot_afford` refusal (audit-gameplay H5, ADR-1809). A cooldown that does not take hold after a
/// paid cost leaves the ability off cooldown (a `chance` or `apply_if` on a cooldown effect is how a
/// design skips it); the run is still charged, so the cost is never paid twice.
fn pay(world: &mut World, owner: Entity, g: GrantId, asset: &AbilityAsset) -> Result<(), Refusal> {
    let level = world
        .get::<AbilityBook>(owner)
        .and_then(|b| b.get(g))
        .map_or(1, |x| x.level);
    let cheats = world
        .get_resource::<AbilitySettings>()
        .map(|s| s.cheats)
        .unwrap_or_default();
    let mut request = |path: &Option<String>, skip: bool| -> bool {
        let Some(path) = path.as_ref().filter(|_| !skip) else {
            return true;
        };
        let req = ApplyEffect::new(path, owner).from_source(owner).at_level(level);
        effects::apply_effect(world, req)
            .is_ok_and(|out| matches!(out.outcome, effects::Outcome::Applied | effects::Outcome::Stacked))
    };
    if !request(&asset.cost, cheats.free_costs) {
        let refusal = Refusal::new(RefusalCause::CannotAfford);
        lifecycle(world, owner, g, LifecycleEvent::Refused(refusal));
        return Err(refusal);
    }
    let _cooldown_took = request(&asset.cooldown, cheats.no_cooldowns);
    if let Some(mut book) = world.get_mut::<AbilityBook>(owner)
        && let Some(run) = book.get_mut(g).and_then(|x| x.run.as_mut())
    {
        run.charged = true;
    }
    lifecycle(world, owner, g, LifecycleEvent::Charged);
    Ok(())
}

/// Ends a run whose `OnStart` charge was refused: the run never paid, so it does not go on.
/// Ignores `cancelable` (this is a failed start, not a cancel).
fn end_unpaid(world: &mut World, owner: Entity, g: GrantId) {
    set_state(world, owner, g, AbilityState::Ending);
    lifecycle(world, owner, g, LifecycleEvent::Cancelled);
    lifecycle(world, owner, g, LifecycleEvent::Ending { cancelled: true });
}

/// Pays cost and cooldown of a running `Explicit` grant. A failed charge returns
/// `cannot_afford` and changes nothing.
pub fn charge(world: &mut World, owner: Entity, g: GrantId) -> Result<(), Refusal> {
    let Some(grant) = world.get::<AbilityBook>(owner).and_then(|b| b.get(g)).cloned() else {
        return Err(Refusal::new(RefusalCause::NotGranted));
    };
    let Some(asset) = assets::ability(world, &grant.asset) else {
        return Err(Refusal::new(RefusalCause::NotGranted));
    };
    let Some(run) = grant
        .run
        .as_ref()
        .filter(|_| grant.state == AbilityState::Running)
    else {
        return Err(Refusal::new(RefusalCause::NotGranted));
    };
    if run.charged {
        return Ok(());
    }
    if run.awaiting_picks {
        return Err(Refusal::new(RefusalCause::TargetRejected));
    }
    let free = world
        .get_resource::<AbilitySettings>()
        .is_some_and(|s| s.cheats.free_costs);
    if !free && let Err(attribute) = can_afford(world, owner, &asset, grant.level) {
        return Err(Refusal {
            cause: RefusalCause::CannotAfford,
            tag: None,
            attribute: Some(attribute),
        });
    }
    pay(world, owner, g, &asset)
}

/// Ends a run (`Running` -> `Ending`); the grant is ready at the next step boundary.
pub fn end(world: &mut World, owner: Entity, g: GrantId) -> Result<(), AbilityError> {
    let mut book = world
        .get_mut::<AbilityBook>(owner)
        .ok_or(AbilityError::GrantNotFound(g.0))?;
    let grant = book.get_mut(g).ok_or(AbilityError::GrantNotFound(g.0))?;
    let running = grant.state == AbilityState::Running;
    if running {
        grant.state = AbilityState::Ending;
    }
    if running {
        lifecycle(world, owner, g, LifecycleEvent::Ending { cancelled: false });
    }
    Ok(())
}

/// Cancels a run (refused with `not_cancelable` when the asset says so).
pub fn cancel(world: &mut World, owner: Entity, g: GrantId) -> Result<(), AbilityError> {
    let grant = world
        .get::<AbilityBook>(owner)
        .and_then(|b| b.get(g))
        .cloned()
        .ok_or(AbilityError::GrantNotFound(g.0))?;
    if grant.state != AbilityState::Running {
        return Ok(());
    }
    let cancelable = assets::ability(world, &grant.asset).is_none_or(|a| a.cancelable);
    if !cancelable {
        return Err(AbilityError::NotCancelable);
    }
    set_state(world, owner, g, AbilityState::Ending);
    lifecycle(world, owner, g, LifecycleEvent::Cancelled);
    lifecycle(world, owner, g, LifecycleEvent::Ending { cancelled: true });
    Ok(())
}

fn set_state(world: &mut World, owner: Entity, g: GrantId, state: AbilityState) {
    if let Some(mut book) = world.get_mut::<AbilityBook>(owner)
        && let Some(x) = book.get_mut(g)
    {
        x.state = state;
    }
}

/// Resolves a run awaiting picks; target requirements are checked (`target_rejected`).
pub fn submit_picks(world: &mut World, owner: Entity, g: GrantId, picks: Picks) -> Result<(), Refusal> {
    let Some(grant) = world.get::<AbilityBook>(owner).and_then(|b| b.get(g)).cloned() else {
        return Err(Refusal::new(RefusalCause::NotGranted));
    };
    let Some(asset) = assets::ability(world, &grant.asset) else {
        return Err(Refusal::new(RefusalCause::NotGranted));
    };
    if !grant.run.as_ref().is_some_and(|r| r.awaiting_picks) {
        return Err(Refusal::new(RefusalCause::NotGranted));
    }
    if let Err(refusal) = check_targets(world, &asset, &picks) {
        lifecycle(world, owner, g, LifecycleEvent::Refused(refusal));
        return Err(refusal);
    }
    if let Some(mut book) = world.get_mut::<AbilityBook>(owner)
        && let Some(run) = book.get_mut(g).and_then(|x| x.run.as_mut())
    {
        run.awaiting_picks = false;
        run.picks = picks;
    }
    lifecycle(world, owner, g, LifecycleEvent::PicksSubmitted);
    if asset.charge == ChargeMode::OnStart
        && let Err(refusal) = pay(world, owner, g, &asset)
    {
        end_unpaid(world, owner, g);
        return Err(refusal);
    }
    Ok(())
}

/// Aborts a run awaiting picks: the run ends (cancelled).
pub fn abort_picks(world: &mut World, owner: Entity, g: GrantId) -> Result<(), AbilityError> {
    let grant = world
        .get::<AbilityBook>(owner)
        .and_then(|b| b.get(g))
        .cloned()
        .ok_or(AbilityError::GrantNotFound(g.0))?;
    if grant.state == AbilityState::Running {
        set_state(world, owner, g, AbilityState::Ending);
        lifecycle(world, owner, g, LifecycleEvent::Ending { cancelled: true });
    }
    Ok(())
}

/// `Ending` -> `Ready`: run tags uncounted, run data dropped per `run_state`, `Ended`, end cues;
/// a one-shot or pending-removal grant is removed. `restart` skips the removal and the queue.
fn finish_run(world: &mut World, owner: Entity, g: GrantId, restart: bool) {
    let Some(grant) = world.get::<AbilityBook>(owner).and_then(|b| b.get(g)).cloned() else {
        return;
    };
    let asset = assets::ability(world, &grant.asset);
    if let Some(asset) = &asset {
        count_tags(world, owner, &asset.run_tags, false);
    }
    if let Some(mut book) = world.get_mut::<AbilityBook>(owner)
        && let Some(x) = book.get_mut(g)
    {
        x.state = AbilityState::Ready;
        x.run = None;
        if asset
            .as_ref()
            .is_none_or(|a| a.run_state != RunStateMode::KeepAcrossRuns)
        {
            x.run_value = None;
        }
    }
    lifecycle(world, owner, g, LifecycleEvent::Ended);
    if let Some(asset) = &asset {
        cues::fire_cues(
            world,
            &asset.cues,
            CueAt::Removed,
            Some(CueKind::End),
            CueBase {
                entity: owner,
                source: Some(owner),
                amount: 1.0,
                effect: None,
            },
        );
    }
    if restart {
        return;
    }
    if grant.one_shot || grant.pending_removal {
        remove_grant(world, owner, g);
        lifecycle(world, owner, g, LifecycleEvent::Revoked);
        return;
    }
    if grant.queued {
        if let Some(mut book) = world.get_mut::<AbilityBook>(owner)
            && let Some(x) = book.get_mut(g)
        {
            x.queued = false;
        }
        let _ = activate(world, owner, g, StartContext::default());
    }
}

/// Writes an [`AbilitySignal`] (matched in `AbilitySet::Triggers`).
pub fn send_signal(world: &mut World, signal: AbilitySignal) {
    world.write_message(signal);
}

/// Matches a signal against the `Signal` triggers of the target's grants now, in ascending
/// `GrantId`; returns the grants that started. No match counts in `signals_unmatched`.
pub fn signal_now(world: &mut World, signal: AbilitySignal) -> Vec<GrantId> {
    let candidates: Vec<GrantId> = world
        .get::<AbilityBook>(signal.target)
        .map(|b| {
            b.grants
                .iter()
                .filter(|x| {
                    let asset = assets::ability(world, &x.asset);
                    asset
                        .iter()
                        .flat_map(|a| a.triggers.iter())
                        .chain(&x.extra_triggers)
                        .any(|t| matches!(t, Trigger::Signal(tag) if signal.tag.matches(*tag)))
                })
                .map(|x| x.id)
                .collect()
        })
        .unwrap_or_default();
    if candidates.is_empty() {
        if let Some(mut stats) = world.get_resource_mut::<AbilityStats>() {
            stats.signals_unmatched += 1;
        }
        return Vec::new();
    }
    let mut started = Vec::new();
    for g in candidates {
        let ctx = StartContext {
            picks: None,
            signal: Some(signal.clone()),
            intent: IntentSeq(0),
        };
        if activate(world, signal.target, g, ctx).is_ok() {
            started.push(g);
        }
    }
    started
}

/// Binds (or unbinds) an input code.
pub fn bind_input(
    world: &mut World,
    owner: Entity,
    g: GrantId,
    code: Option<u16>,
) -> Result<(), AbilityError> {
    let mut book = world
        .get_mut::<AbilityBook>(owner)
        .ok_or(AbilityError::GrantNotFound(g.0))?;
    book.get_mut(g)
        .ok_or(AbilityError::GrantNotFound(g.0))?
        .input_code = code;
    Ok(())
}

/// Blocks or unblocks an input code on an entity.
pub fn block_input(world: &mut World, owner: Entity, code: u16, blocked: bool) {
    if let Some(mut book) = world.get_mut::<AbilityBook>(owner) {
        book.blocked_inputs.retain(|c| *c != code);
        if blocked {
            book.blocked_inputs.push(code);
            book.blocked_inputs.sort_unstable();
        }
    }
}

/// The grant's run-state value.
#[must_use]
pub fn run_value<T: DeserializeOwned>(world: &World, owner: Entity, g: GrantId) -> Option<T> {
    let text = world.get::<AbilityBook>(owner)?.get(g)?.run_value.as_deref()?;
    ron::from_str(text).ok()
}

/// Replaces the grant's run-state value.
pub fn set_run_value<T: Serialize>(
    world: &mut World,
    owner: Entity,
    g: GrantId,
    value: &T,
) -> Result<(), AbilityError> {
    let text = ron::to_string(value).map_err(|e| AbilityError::Format(e.to_string()))?;
    let mut book = world
        .get_mut::<AbilityBook>(owner)
        .ok_or(AbilityError::GrantNotFound(g.0))?;
    book.get_mut(g).ok_or(AbilityError::GrantNotFound(g.0))?.run_value = Some(text);
    Ok(())
}

/// Replaces the grant's scratch numbers.
pub fn set_scratch(
    world: &mut World,
    owner: Entity,
    g: GrantId,
    scratch: [f32; 4],
) -> Result<(), AbilityError> {
    let mut book = world
        .get_mut::<AbilityBook>(owner)
        .ok_or(AbilityError::GrantNotFound(g.0))?;
    book.get_mut(g).ok_or(AbilityError::GrantNotFound(g.0))?.scratch = scratch;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Passes.

/// The next messages of `cursor`, at most [`MAX_MESSAGES_PER_STEP`]. The rest of a step's burst is
/// dropped with one `message_budget` warning: later messages cannot wait for a later step, because
/// the queue only keeps them for two updates, and dropping the tail keeps the outcome deterministic.
fn drain<M: Message + Clone>(world: &mut World, cursor: &mut MessageCursor<M>) -> Vec<M> {
    let Some(messages) = world.get_resource::<Messages<M>>() else {
        return Vec::new();
    };
    let taken: Vec<M> = cursor
        .read(messages)
        .take(MAX_MESSAGES_PER_STEP)
        .cloned()
        .collect();
    if cursor.read(messages).next().is_some() {
        let dropped = cursor.read(messages).count() + 1;
        tracing::warn!(target: "regia_framework", dropped, "abilities message budget exceeded");
        effects::warn(world, "message_budget");
    }
    taken
}

/// `AbilitySet::Input`: presses start bound grants; releases write `InputReleased`.
pub(crate) fn input_pass(world: &mut World, mut cursor: Local<MessageCursor<AbilityInput>>) {
    for input in drain(world, &mut cursor) {
        let bound: Vec<(GrantId, AbilityState)> = world
            .get::<AbilityBook>(input.entity)
            .map(|b| {
                b.grants
                    .iter()
                    .filter(|x| x.input_code == Some(input.code) && !x.pending_removal)
                    .map(|x| (x.id, x.state))
                    .collect()
            })
            .unwrap_or_default();
        let one = world
            .get::<AbilityBook>(input.entity)
            .is_some_and(|b| b.one_per_press);
        for (g, state) in bound {
            if input.down {
                // Only a run that started consumes a `one_per_press` press: a `QueueOne` grant that
                // was running only queued one, so the next bound grant still gets the press
                // (ADR-1809).
                let ok = activate(world, input.entity, g, StartContext::default()).is_ok();
                let queued = world
                    .get::<AbilityBook>(input.entity)
                    .and_then(|b| b.get(g))
                    .is_some_and(|x| x.queued);
                if ok && !queued && one {
                    break;
                }
            } else if state == AbilityState::Running {
                lifecycle(world, input.entity, g, LifecycleEvent::InputReleased);
            }
        }
    }
}

/// Cursors of the request messages.
#[derive(Default)]
pub(crate) struct RequestCursors {
    start: MessageCursor<StartAbility>,
    cancel: MessageCursor<CancelAbility>,
    revoke: MessageCursor<RevokeAbility>,
    apply: MessageCursor<ApplyEffect>,
}

/// `AbilitySet::Requests`: apply, start, cancel and revoke requests, each kind in message order.
/// The kinds themselves are processed in the fixed order apply -> start -> cancel -> revoke
/// (ADR-7573): each kind is its own message queue, so a cross-kind write order cannot be
/// recovered, and the fixed order is the contract.
pub(crate) fn requests_pass(world: &mut World, mut cursors: Local<RequestCursors>) {
    let _span = tracing::info_span!(target: "regia_framework", "abilities_requests").entered();
    for req in drain(world, &mut cursors.apply) {
        let _ = effects::apply_effect(world, req);
    }
    for req in drain(world, &mut cursors.start) {
        let _ = activate(world, req.owner, req.grant, req.context);
    }
    for req in drain(world, &mut cursors.cancel) {
        let _ = cancel(world, req.owner, req.grant);
    }
    for req in drain(world, &mut cursors.revoke) {
        let _ = revoke_ability(world, req.owner, req.grant, req.defer);
    }
}

fn mix_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn mix_u64(hash: u64, value: u64) -> u64 {
    mix_bytes(hash, &value.to_le_bytes())
}

fn tag_fingerprint(tags: &[Tag]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    for tag in tags {
        hash = mix_u64(hash, tag.id());
    }
    hash
}

fn block_fingerprint(book: &AbilityBook, self_id: GrantId) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    for other in book.grants.iter().filter(|grant| {
        grant.id != self_id && matches!(grant.state, AbilityState::Running | AbilityState::Ending)
    }) {
        hash = mix_u64(hash, u64::from(other.id.0));
        let state = match other.state {
            AbilityState::Running => 1_u64,
            AbilityState::Ending => 2,
            AbilityState::Ready | AbilityState::Cooling => 0,
        };
        hash = mix_u64(hash, state);
        hash = mix_bytes(hash, other.asset.as_bytes());
    }
    hash
}

fn attribute_tick(world: &World, owner: Entity) -> u32 {
    world
        .get_entity(owner)
        .ok()
        .and_then(|entity| entity.get_ref::<Attributes>())
        .map(|attrs| attrs.last_changed().get())
        .unwrap_or(0)
}

fn refresh_step_cache(world: &mut World, owner: Entity, g: GrantId) {
    let Some(path) = world
        .get::<AbilityBook>(owner)
        .and_then(|book| book.get(g))
        .map(|grant| grant.asset.clone())
    else {
        return;
    };
    let generation = world
        .get_resource::<assets::AbilityAssets>()
        .map(|assets| assets.generation())
        .unwrap_or(0);
    let data = world.get_resource::<assets::AbilityAssets>().and_then(|assets| {
        assets.ability_ref(&path).map(|asset| GrantStepData {
            triggers: asset.triggers.clone(),
            max_run_ticks: asset.max_run_ticks,
            pick_timeout_ticks: asset.pick_timeout_ticks,
            asset_gen: generation,
        })
    });
    if let Some(mut book) = world.get_mut::<AbilityBook>(owner)
        && let Some(grant) = book.get_mut(g)
    {
        grant.step_cache = data;
    }
}

fn hold_cause_cached(cause: RefusalCause) -> bool {
    matches!(
        cause,
        RefusalCause::OnCooldown
            | RefusalCause::OwnerMissingTag
            | RefusalCause::OwnerForbiddenTag
            | RefusalCause::BlockedByAbility
            | RefusalCause::CannotAfford
            | RefusalCause::OwnerLocked
            | RefusalCause::LevelZero
    )
}

/// The asset generation and cheat switches a held refusal depends on.
fn hold_context(world: &World) -> (u64, Cheats) {
    let generation = world
        .get_resource::<assets::AbilityAssets>()
        .map_or(0, |assets| assets.generation());
    let cheats = world
        .get_resource::<AbilitySettings>()
        .map(|s| s.cheats)
        .unwrap_or_default();
    (generation, cheats)
}

fn activation_held(world: &World, owner: Entity, g: GrantId, tags_fp: u64) -> bool {
    let Some((hold, level, blocks)) = (|| {
        let book = world.get::<AbilityBook>(owner)?;
        let grant = book.get(g)?;
        let hold = grant.while_hold?;
        Some((hold, grant.level, block_fingerprint(book, g)))
    })() else {
        return false;
    };
    if (hold.asset_gen, hold.cheats) != hold_context(world) {
        return false;
    }
    match hold.cause {
        RefusalCause::OnCooldown | RefusalCause::OwnerMissingTag | RefusalCause::OwnerForbiddenTag => {
            hold.tags_fp == tags_fp
        }
        RefusalCause::BlockedByAbility => hold.tags_fp == tags_fp && hold.block_fp == blocks,
        RefusalCause::CannotAfford => {
            hold.tags_fp == tags_fp && hold.level == level && hold.attr_tick == attribute_tick(world, owner)
        }
        RefusalCause::OwnerLocked => world.get::<AbilitiesLocked>(owner).is_some(),
        RefusalCause::LevelZero => level == 0,
        _ => false,
    }
}

fn clear_while_hold(world: &mut World, owner: Entity, g: GrantId) {
    if let Some(mut book) = world.get_mut::<AbilityBook>(owner)
        && let Some(grant) = book.get_mut(g)
    {
        grant.while_hold = None;
    }
}

/// Remembers a `WhileTagged` refusal. `tags_fp` fingerprints the tags at the start of the owner's
/// turn, which is what [`activation_held`] compares later; the refusal itself was computed on the
/// tags at check time. When an earlier grant of this turn changed the tags in between, the
/// refusal is not cached (it may not hold for the turn-start tags). Likewise a `CannotAfford`
/// refusal is not cached when the attributes already changed during this system run: a later
/// change in the same run would carry the same change tick and go unnoticed (corr-framework-a F1).
fn remember_while_hold(world: &mut World, owner: Entity, g: GrantId, cause: RefusalCause, tags_fp: u64) {
    if !hold_cause_cached(cause) {
        clear_while_hold(world, owner, g);
        return;
    }
    let mut now_tags = Vec::new();
    effective_tags_into(world, owner, &mut now_tags);
    let attr_tick = attribute_tick(world, owner);
    let attrs_moved_this_run =
        cause == RefusalCause::CannotAfford && attr_tick == world.read_change_tick().get();
    if tag_fingerprint(&now_tags) != tags_fp || attrs_moved_this_run {
        clear_while_hold(world, owner, g);
        return;
    }
    let (asset_gen, cheats) = hold_context(world);
    let blocks = world
        .get::<AbilityBook>(owner)
        .map_or(0, |book| block_fingerprint(book, g));
    if let Some(mut book) = world.get_mut::<AbilityBook>(owner)
        && let Some(grant) = book.get_mut(g)
    {
        let level = grant.level;
        grant.while_hold = Some(WhileHold {
            cause,
            tags_fp,
            attr_tick,
            block_fp: blocks,
            asset_gen,
            cheats,
            level,
        });
    }
}

fn step_limits(world: &mut World, owner: Entity, g: GrantId) -> Option<(Option<u32>, Option<u32>)> {
    let generation = world
        .get_resource::<assets::AbilityAssets>()
        .map(|assets| assets.generation())
        .unwrap_or(0);
    let cached = world.get::<AbilityBook>(owner).and_then(|book| {
        let grant = book.get(g)?;
        let cache = grant
            .step_cache
            .as_ref()
            .filter(|cache| cache.asset_gen == generation)?;
        Some((cache.pick_timeout_ticks, cache.max_run_ticks))
    });
    if cached.is_some() {
        return cached;
    }
    refresh_step_cache(world, owner, g);
    world.get::<AbilityBook>(owner).and_then(|book| {
        let cache = book.get(g)?.step_cache.as_ref()?;
        Some((cache.pick_timeout_ticks, cache.max_run_ticks))
    })
}

/// Buffers of the trigger pass, kept between steps.
#[derive(Default)]
pub(crate) struct TriggerScratch {
    order: EntityOrder,
    tags: Vec<Tag>,
    gained: Vec<Tag>,
    triggers: Vec<(GrantId, AbilityState, Option<Tag>, Trigger)>,
}

/// `AbilitySet::Triggers`: signals, then `TagGained` and `WhileTagged` triggers.
pub(crate) fn triggers_pass(
    world: &mut World,
    query: &mut QueryState<OrderItem, With<AbilityBook>>,
    mut cursor: Local<MessageCursor<AbilitySignal>>,
    mut scratch: Local<TriggerScratch>,
) {
    let _span = tracing::info_span!(target: "regia_framework", "abilities_triggers").entered();
    for signal in drain(world, &mut cursor) {
        signal_now(world, signal);
    }
    let scratch = &mut *scratch;
    let ordered = scratch.order.take(world, query);
    for &owner in &ordered {
        effective_tags_into(world, owner, &mut scratch.tags);
        scratch.gained.clear();
        scratch.triggers.clear();
        let asset_gen = world
            .get_resource::<assets::AbilityAssets>()
            .map(|assets| assets.generation())
            .unwrap_or(0);
        let Some(book) = world.get::<AbilityBook>(owner) else {
            continue;
        };
        let mut stale: Vec<GrantId> = Vec::new();
        for x in &book.grants {
            let while_tag = x.run.as_ref().and_then(|r| r.while_tag);
            let row = |t: &Trigger| (x.id, x.state, while_tag, *t);
            match x.step_cache.as_ref() {
                Some(cache) if cache.asset_gen == asset_gen => {
                    scratch.triggers.extend(cache.triggers.iter().map(row));
                }
                _ => stale.push(x.id),
            }
            scratch.triggers.extend(x.extra_triggers.iter().map(row));
        }
        if book.seen_tags.as_ref() != Some(&scratch.tags) {
            if let Some(before) = &book.seen_tags {
                let gained = scratch.tags.iter().filter(|t| !before.contains(t));
                scratch.gained.extend(gained.copied());
            }
            if let Some(mut book) = world.get_mut::<AbilityBook>(owner) {
                book.seen_tags = Some(scratch.tags.clone());
            }
        }
        for g in stale {
            refresh_step_cache(world, owner, g);
            if let Some(book) = world.get::<AbilityBook>(owner)
                && let Some(grant) = book.get(g)
                && let Some(cache) = grant.step_cache.as_ref()
            {
                let while_tag = grant.run.as_ref().and_then(|run| run.while_tag);
                let state = grant.state;
                for trigger in &cache.triggers {
                    scratch.triggers.push((g, state, while_tag, *trigger));
                }
            }
        }
        let tags_fp = tag_fingerprint(&scratch.tags);
        for &(g, state, while_tag, trigger) in &scratch.triggers {
            match trigger {
                Trigger::Signal(_) => {}
                Trigger::TagGained(tag) => {
                    if state == AbilityState::Ready && scratch.gained.iter().any(|x| x.matches(tag)) {
                        let _ = activate(world, owner, g, StartContext::default());
                    }
                }
                Trigger::WhileTagged(tag) => {
                    // The tags at the start of this owner's turn, like `TagGained` above.
                    let holds = scratch.tags.iter().any(|x| x.matches(tag));
                    if holds && state == AbilityState::Ready {
                        if !activation_held(world, owner, g, tags_fp) {
                            match check_activation(world, owner, g, &StartContext::default()) {
                                Ok(()) => {
                                    clear_while_hold(world, owner, g);
                                    if start_run(world, owner, g, StartContext::default()).is_ok()
                                        && let Some(mut book) = world.get_mut::<AbilityBook>(owner)
                                        && let Some(run) = book.get_mut(g).and_then(|x| x.run.as_mut())
                                    {
                                        run.while_tag = Some(tag);
                                    }
                                }
                                Err(refusal) => {
                                    lifecycle(world, owner, g, LifecycleEvent::Refused(refusal));
                                    remember_while_hold(world, owner, g, refusal.cause, tags_fp);
                                }
                            }
                        }
                    } else if !holds {
                        clear_while_hold(world, owner, g);
                        if state == AbilityState::Running && while_tag == Some(tag) {
                            let _ = end(world, owner, g);
                        }
                    }
                }
            }
        }
    }
    scratch.order.give_back(ordered);
}

/// One grant the state pass acts on: id, state, run start tick, waiting for picks.
type StepGrant = (GrantId, AbilityState, u64, bool);

/// Buffers of the state pass, kept between steps.
#[derive(Default)]
pub(crate) struct StatesScratch {
    order: EntityOrder,
    grants: Vec<StepGrant>,
}

/// `AbilitySet::States`: `Ending` -> `Ready`, `max_run_ticks`, `pick_timeout_ticks`.
pub(crate) fn states_pass(
    world: &mut World,
    query: &mut QueryState<OrderItem, With<AbilityBook>>,
    mut scratch: Local<StatesScratch>,
) {
    let _span = tracing::info_span!(target: "regia_framework", "abilities_states").entered();
    let now = tick(world);
    let scratch = &mut *scratch;
    let ordered = scratch.order.take(world, query);
    for &owner in &ordered {
        scratch.grants.clear();
        if let Some(book) = world.get::<AbilityBook>(owner) {
            let acting = book.grants.iter().filter_map(|x| match (x.state, &x.run) {
                (AbilityState::Ending, _) => Some((x.id, x.state, 0, false)),
                (AbilityState::Running, Some(run)) => {
                    Some((x.id, x.state, run.started_tick, run.awaiting_picks))
                }
                _ => None,
            });
            scratch.grants.extend(acting);
        }
        for &(g, state, started_tick, awaiting_picks) in &scratch.grants {
            if state == AbilityState::Ending {
                finish_run(world, owner, g, false);
                continue;
            }
            let Some((pick_timeout_ticks, max_run_ticks)) = step_limits(world, owner, g) else {
                continue;
            };
            let age = now.saturating_sub(started_tick);
            if awaiting_picks
                && let Some(limit) = pick_timeout_ticks
                && age >= u64::from(limit)
            {
                lifecycle(
                    world,
                    owner,
                    g,
                    LifecycleEvent::Refused(Refusal::new(RefusalCause::PickTimeout)),
                );
                set_state(world, owner, g, AbilityState::Ending);
                lifecycle(world, owner, g, LifecycleEvent::Ending { cancelled: true });
            } else if let Some(limit) = max_run_ticks
                && age >= u64::from(limit)
            {
                let _ = end(world, owner, g);
            }
        }
    }
    scratch.order.give_back(ordered);
}
