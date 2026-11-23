// SPDX-License-Identifier: MPL-2.0
//! Smart objects v2 (`docs/specs/39-smart-objects-v2.md`, ADR-1638): claim priorities with
//! generation-checked handles, a claim life cycle (`Free -> Claimed -> InUse -> Free`) with automatic
//! release, filtered search with a rejection log, entrances, runtime tags, off-reasons and change
//! messages. It extends [`crate::smart_object`] additively: `claim_slot` / `release_slot` keep their
//! signatures and results and delegate here at [`ClaimPriority::Normal`].
//!
//! # How the pieces fit
//!
//! * [`claim_with`], [`begin_use`] and [`release_handle`] change a slot's claim state immediately and
//!   queue a [`SmartObjectEvent`]. The queued events are written as the bevy messages
//!   [`SlotChanged`] and [`SlotLost`] in `PostUpdate`, ordered by object entity bits and then slot
//!   entity bits, and are also kept in a bounded log ([`SmartObjectEvents::recent`]) for the agent.
//! * [`SmartObjectIndex::search`] filters slots in seven stages (region, enabled, state, family,
//!   activity, user, condition); the first failing stage is the recorded [`Rejection`].
//! * Objects carry the shared part of a definition in [`SmartObjectRules`] and
//!   [`SmartObjectConditions`]; slots carry [`SlotBehaviors`]. Hand-built content and the
//!   definition loader fill the same components, so search does not care where they came from.
//!
//! # Deviations from the spec text
//!
//! * `SlotLost` also names the `holder` that lost the claim (a handle alone does not say who).
//! * Behavior payloads decode through `serde` (`register_behavior::<T: DeserializeOwned>`), not
//!   through reflection.
//! * `ClaimHandle::to_bits` packs the slot entity index with the claim generation XOR the
//!   entity generation; `ClaimHandle::from_bits` finds the live slot with that index.

use std::any::{Any, TypeId};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Mutex;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::tags::{Tag, TagContainer, TagLookup, TagQuery};
use regia_nav::NavQuery;
use regia_scene::GlobalTransform;
use serde::de::DeserializeOwned;

use crate::smart_object::{
    SlotInfo, SmartObject, SmartObjectError, SmartObjectIndex, SmartObjectSlot, effective_enabled,
    smart_object_ancestor,
};

/// The most events [`SmartObjectEvents`] keeps for `smartobject.messages`.
pub const MAX_EVENT_LOG: usize = 256;

// ---- priorities, handles, state ---------------------------------------------------------

/// How strongly a claim holds a slot. A claim can only displace a holder of a strictly lower
/// priority.
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ClaimPriority {
    /// Idle behavior that anyone may displace.
    Background = 0,
    /// Low importance.
    Low = 1,
    /// The default for ordinary claims.
    #[default]
    Normal = 2,
    /// Important.
    High = 3,
    /// Displaces everything except an in-use slot.
    Urgent = 4,
}

impl ClaimPriority {
    /// Every level, lowest first.
    pub const ALL: [ClaimPriority; 5] = [
        Self::Background,
        Self::Low,
        Self::Normal,
        Self::High,
        Self::Urgent,
    ];

    /// The lowercase name used by the agent protocol.
    pub fn name(self) -> &'static str {
        match self {
            Self::Background => "background",
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
            Self::Urgent => "urgent",
        }
    }

    /// The level named `name` (case-insensitive), if any.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|p| p.name().eq_ignore_ascii_case(name))
    }
}

/// A claim on one slot. The `generation` increments whenever the slot is released or invalidated,
/// so a handle that outlived its claim never matches a later one. (The packed `u64` form also
/// mixes in the slot entity's generation; see [`ClaimHandle::to_bits`].)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClaimHandle {
    /// The claimed slot.
    pub slot: Entity,
    /// The slot's claim generation when the handle was issued.
    pub generation: u32,
}

const INDEX_MASK: u64 = 0xFFFF_FFFF;

impl ClaimHandle {
    /// Packs the handle into a `u64` (for a blackboard `Int` key): the slot entity's index in the
    /// low half, the claim generation XOR the entity's own generation in the high half. The XOR
    /// makes a handle to a despawned slot decode to a different claim generation when its index is
    /// reused, even if both slots are at their first claim (generation 0).
    pub fn to_bits(self) -> u64 {
        let entity_generation = self.slot.to_bits() >> 32;
        (self.slot.to_bits() & INDEX_MASK) | ((u64::from(self.generation) ^ entity_generation) << 32)
    }

    /// Unpacks [`ClaimHandle::to_bits`], finding the live slot entity with that index. `None` when
    /// no slot matches.
    pub fn from_bits(world: &mut World, bits: u64) -> Option<Self> {
        let index = bits & INDEX_MASK;
        let mixed = (bits >> 32) as u32;
        let mut query = world.query::<(Entity, &SmartObjectSlot)>();
        query
            .iter(world)
            .map(|(entity, _)| entity)
            .find(|entity| entity.to_bits() & INDEX_MASK == index)
            .map(|slot| Self {
                slot,
                generation: mixed ^ (slot.to_bits() >> 32) as u32,
            })
    }
}

/// The derived state of a slot (never stored twice).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    /// Enabled and unclaimed.
    Free,
    /// Claimed, not yet used.
    Claimed,
    /// The holder is using it.
    InUse,
    /// The slot, its object or an off-reason says so.
    Disabled,
}

impl SlotState {
    /// The lowercase name used by the agent protocol.
    pub fn name(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Claimed => "claimed",
            Self::InUse => "in_use",
            Self::Disabled => "disabled",
        }
    }
}

/// The state of `slot`, or `None` when it is not a slot.
pub fn slot_state(world: &World, slot: Entity) -> Option<SlotState> {
    let state = world.get::<SmartObjectSlot>(slot)?;
    Some(if !effective_enabled(world, slot) {
        SlotState::Disabled
    } else if state.in_use {
        SlotState::InUse
    } else if state.claimed_by.is_some() {
        SlotState::Claimed
    } else {
        SlotState::Free
    })
}

// ---- events -----------------------------------------------------------------------------

/// What changed in a [`SlotChanged`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotChange {
    /// A claim was made.
    Claimed,
    /// The holder began using the slot.
    InUse,
    /// The claim was released.
    Released,
    /// A runtime tag was added.
    TagAdded,
    /// A runtime tag was removed.
    TagRemoved,
    /// The effective enabled value became true.
    Enabled,
    /// The effective enabled value became false.
    Disabled,
    /// A custom notification (`notify_slot`).
    Custom,
}

impl SlotChange {
    /// The lowercase name used by the agent protocol.
    pub fn name(self) -> &'static str {
        match self {
            Self::Claimed => "claimed",
            Self::InUse => "in_use",
            Self::Released => "released",
            Self::TagAdded => "tag_added",
            Self::TagRemoved => "tag_removed",
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::Custom => "custom",
        }
    }
}

/// Why a holder lost its claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LostReason {
    /// A higher-priority claim displaced it.
    Preempted,
    /// The slot or its object became disabled.
    Disabled,
    /// The holder entity no longer exists.
    HolderGone,
    /// The slot or its object no longer exists.
    SlotGone,
    /// A definition reload removed the slot.
    SlotRemoved,
}

impl LostReason {
    /// The lowercase name used by the agent protocol.
    pub fn name(self) -> &'static str {
        match self {
            Self::Preempted => "preempted",
            Self::Disabled => "disabled",
            Self::HolderGone => "holder_gone",
            Self::SlotGone => "slot_gone",
            Self::SlotRemoved => "slot_removed",
        }
    }
}

/// A slot or object changed. Written (as a message) in `PostUpdate`, only when the effective value
/// changed.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct SlotChanged {
    /// The owning object (for a slot) or the object itself.
    pub object: Option<Entity>,
    /// The slot, or `None` for an object-scope change.
    pub slot: Option<Entity>,
    /// What changed.
    pub what: SlotChange,
    /// The tag involved (tag changes and custom notifications).
    pub tag: Option<Tag>,
}

/// A holder lost its claim; the claim is already cleared when this is read.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct SlotLost {
    /// The handle that is now stale.
    pub handle: ClaimHandle,
    /// The entity that held the claim.
    pub holder: Entity,
    /// Why.
    pub reason: LostReason,
}

/// One queued or logged smart-object message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmartObjectEvent {
    /// A [`SlotChanged`].
    Changed(SlotChanged),
    /// A [`SlotLost`].
    Lost(SlotLost),
}

/// The pending events and the bounded log of recent ones (added by [`crate::SmartObjectPlugin`]).
#[derive(Resource, Default)]
pub struct SmartObjectEvents {
    pending: Vec<((u64, u64), SmartObjectEvent)>,
    log: VecDeque<SmartObjectEvent>,
}

impl SmartObjectEvents {
    /// The most recent `last` logged events (at most [`MAX_EVENT_LOG`]), oldest first.
    pub fn recent(&self, last: usize) -> Vec<SmartObjectEvent> {
        let last = last.min(MAX_EVENT_LOG);
        let skip = self.log.len().saturating_sub(last);
        self.log.iter().skip(skip).cloned().collect()
    }

    /// How many events wait for the next `PostUpdate` flush.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

fn emit(world: &mut World, event: SmartObjectEvent) {
    let key = match &event {
        SmartObjectEvent::Changed(c) => (
            c.object.map_or(0, Entity::to_bits),
            c.slot.map_or(0, Entity::to_bits),
        ),
        SmartObjectEvent::Lost(l) => (
            smart_object_ancestor(world, l.handle.slot).map_or(0, Entity::to_bits),
            l.handle.slot.to_bits(),
        ),
    };
    if let Some(mut events) = world.get_resource_mut::<SmartObjectEvents>() {
        events.pending.push((key, event));
    }
}

fn emit_changed(world: &mut World, target: Entity, what: SlotChange, tag: Option<Tag>) {
    let is_slot = world.get::<SmartObjectSlot>(target).is_some();
    let (object, slot) = if is_slot {
        (smart_object_ancestor(world, target), Some(target))
    } else {
        (Some(target), None)
    };
    emit(
        world,
        SmartObjectEvent::Changed(SlotChanged {
            object,
            slot,
            what,
            tag,
        }),
    );
}

/// `PostUpdate`: writes the queued events as messages (object bits, then slot bits; arrival order
/// within one slot) and appends them to the log.
pub(crate) fn flush_smart_object_events(world: &mut World) {
    let _span = tracing::debug_span!("flush_smart_object_events").entered();
    let batch = {
        let Some(mut events) = world.get_resource_mut::<SmartObjectEvents>() else {
            return;
        };
        if events.pending.is_empty() {
            return;
        }
        let mut batch = std::mem::take(&mut events.pending);
        batch.sort_by_key(|(key, _)| *key);
        for (_, event) in &batch {
            if events.log.len() >= MAX_EVENT_LOG {
                events.log.pop_front();
            }
            events.log.push_back(event.clone());
        }
        batch
    };
    for (_, event) in batch {
        match event {
            SmartObjectEvent::Changed(changed) => {
                let _ = world.write_message(changed);
            }
            SmartObjectEvent::Lost(lost) => {
                let _ = world.write_message(lost);
            }
        }
    }
}

// ---- claim life cycle --------------------------------------------------------------------

/// Live claims by slot entity bits, so a despawned slot or holder can still be reported.
#[derive(Resource, Default)]
pub(crate) struct ClaimLedger {
    claims: BTreeMap<u64, (Entity, Entity, u32)>,
}

fn ledger_insert(world: &mut World, slot: Entity, holder: Entity, generation: u32) {
    if let Some(mut ledger) = world.get_resource_mut::<ClaimLedger>() {
        ledger.claims.insert(slot.to_bits(), (slot, holder, generation));
    }
}

fn ledger_remove(world: &mut World, slot: Entity) {
    if let Some(mut ledger) = world.get_resource_mut::<ClaimLedger>() {
        ledger.claims.remove(&slot.to_bits());
    }
}

/// Claims `slot` for `user` at `priority`.
///
/// A free slot is claimed. A slot held by another user is taken over only at a strictly higher
/// priority (the old holder gets [`SlotLost`] with [`LostReason::Preempted`] and its handle goes
/// stale); otherwise [`SmartObjectError::SlotClaimed`]. An in-use slot answers
/// [`SmartObjectError::SlotInUse`] to anyone but its holder, a disabled one
/// [`SmartObjectError::SlotDisabled`]. The same user claiming again gets the same handle and may
/// raise the held priority.
pub fn claim_with(
    world: &mut World,
    slot: Entity,
    user: Entity,
    priority: ClaimPriority,
) -> Result<ClaimHandle, SmartObjectError> {
    let Some(state) = world.get::<SmartObjectSlot>(slot) else {
        return Err(SmartObjectError::SlotNotFound(slot));
    };
    let (holder, held, in_use, generation) = (
        state.claimed_by,
        state.priority_held,
        state.in_use,
        state.generation,
    );
    if !effective_enabled(world, slot) {
        return Err(SmartObjectError::SlotDisabled(slot));
    }
    let mut next_generation = generation;
    match holder {
        Some(current) if current == user => {
            if priority > held
                && let Some(mut state) = world.get_mut::<SmartObjectSlot>(slot)
            {
                state.priority_held = priority;
            }
            return Ok(ClaimHandle { slot, generation });
        }
        Some(current) => {
            if in_use {
                return Err(SmartObjectError::SlotInUse(slot));
            }
            if priority <= held {
                return Err(SmartObjectError::SlotClaimed(slot, current));
            }
            emit(
                world,
                SmartObjectEvent::Lost(SlotLost {
                    handle: ClaimHandle { slot, generation },
                    holder: current,
                    reason: LostReason::Preempted,
                }),
            );
            next_generation = generation.wrapping_add(1);
        }
        None => {}
    }
    let Some(mut state) = world.get_mut::<SmartObjectSlot>(slot) else {
        return Err(SmartObjectError::SlotNotFound(slot));
    };
    state.claimed_by = Some(user);
    state.priority_held = priority;
    state.in_use = false;
    state.generation = next_generation;
    ledger_insert(world, slot, user, next_generation);
    emit_changed(world, slot, SlotChange::Claimed, None);
    Ok(ClaimHandle {
        slot,
        generation: next_generation,
    })
}

fn check_handle(world: &World, handle: ClaimHandle) -> Result<(), SmartObjectError> {
    let Some(state) = world.get::<SmartObjectSlot>(handle.slot) else {
        return Err(SmartObjectError::SlotNotFound(handle.slot));
    };
    if state.claimed_by.is_none() || state.generation != handle.generation {
        return Err(SmartObjectError::StaleHandle);
    }
    Ok(())
}

/// `Claimed -> InUse`. Idempotent. A stale handle is [`SmartObjectError::StaleHandle`].
pub fn begin_use(world: &mut World, handle: ClaimHandle) -> Result<(), SmartObjectError> {
    check_handle(world, handle)?;
    if !effective_enabled(world, handle.slot) {
        return Err(SmartObjectError::SlotDisabled(handle.slot));
    }
    let Some(mut state) = world.get_mut::<SmartObjectSlot>(handle.slot) else {
        return Err(SmartObjectError::SlotNotFound(handle.slot));
    };
    if state.in_use {
        return Ok(());
    }
    state.in_use = true;
    emit_changed(world, handle.slot, SlotChange::InUse, None);
    Ok(())
}

/// `Claimed | InUse -> Free`. A stale handle is [`SmartObjectError::StaleHandle`].
pub fn release_handle(world: &mut World, handle: ClaimHandle) -> Result<(), SmartObjectError> {
    check_handle(world, handle)?;
    release_claim(world, handle.slot)
}

/// Like [`release_handle`], but refuses unless `user` holds the claim
/// ([`SmartObjectError::NotHolder`]).
pub fn release_handle_as(
    world: &mut World,
    handle: ClaimHandle,
    user: Entity,
) -> Result<(), SmartObjectError> {
    check_handle(world, handle)?;
    let holder = world
        .get::<SmartObjectSlot>(handle.slot)
        .and_then(|s| s.claimed_by);
    if holder != Some(user) {
        return Err(SmartObjectError::NotHolder);
    }
    release_claim(world, handle.slot)
}

/// Clears the claim on `slot` (a no-op on an unclaimed slot), bumps the generation and queues a
/// `Released` change.
pub(crate) fn release_claim(world: &mut World, slot: Entity) -> Result<(), SmartObjectError> {
    {
        let Some(mut state) = world.get_mut::<SmartObjectSlot>(slot) else {
            return Err(SmartObjectError::SlotNotFound(slot));
        };
        if state.claimed_by.is_none() {
            return Ok(());
        }
        state.claimed_by = None;
        state.in_use = false;
        state.priority_held = ClaimPriority::Normal;
        state.generation = state.generation.wrapping_add(1);
    }
    ledger_remove(world, slot);
    emit_changed(world, slot, SlotChange::Released, None);
    Ok(())
}

/// Clears the claim recorded in the ledger because `reason` invalidated it, and queues one
/// [`SlotLost`] (plus a `Released` change when the slot still exists).
pub(crate) fn invalidate_claim(
    world: &mut World,
    slot: Entity,
    holder: Entity,
    generation: u32,
    reason: LostReason,
) {
    let mut slot_exists = false;
    if let Some(mut state) = world.get_mut::<SmartObjectSlot>(slot) {
        slot_exists = true;
        state.claimed_by = None;
        state.in_use = false;
        state.priority_held = ClaimPriority::Normal;
        state.generation = generation.wrapping_add(1);
    }
    ledger_remove(world, slot);
    emit(
        world,
        SmartObjectEvent::Lost(SlotLost {
            handle: ClaimHandle { slot, generation },
            holder,
            reason,
        }),
    );
    if slot_exists {
        emit_changed(world, slot, SlotChange::Released, None);
    }
}

/// `PostUpdate`, before the index rebuild: releases claims whose holder, slot or object is gone or
/// whose slot became disabled, writing one [`SlotLost`] each (ordered by slot entity bits).
#[tracing::instrument(skip_all, target = "regia_ai", name = "smart_object_auto_release")]
pub(crate) fn smart_object_auto_release(world: &mut World) {
    let _span = tracing::debug_span!("smart_object_auto_release").entered();
    let Some(ledger) = world.get_resource::<ClaimLedger>() else {
        return;
    };
    if ledger.claims.is_empty() {
        return;
    }
    let claims: Vec<(Entity, Entity, u32)> = ledger.claims.values().copied().collect();
    for (slot, holder, generation) in claims {
        let reason = if world.get::<SmartObjectSlot>(slot).is_none() {
            Some(LostReason::SlotGone)
        } else if world.get_entity(holder).is_err() {
            Some(LostReason::HolderGone)
        } else if !effective_enabled(world, slot) {
            Some(LostReason::Disabled)
        } else {
            None
        };
        if let Some(reason) = reason {
            invalidate_claim(world, slot, holder, generation, reason);
        }
    }
}

// ---- tags, enabling, notifications -------------------------------------------------------

/// Adds a runtime tag to a slot or an object. Returns whether anything changed.
pub fn add_runtime_tag(world: &mut World, target: Entity, tag: Tag) -> bool {
    let changed = if let Some(mut slot) = world.get_mut::<SmartObjectSlot>(target) {
        slot.runtime_tags.add(tag)
    } else if let Some(mut object) = world.get_mut::<SmartObject>(target) {
        object.runtime_tags.add(tag)
    } else {
        false
    };
    if changed {
        emit_changed(world, target, SlotChange::TagAdded, Some(tag));
    }
    changed
}

/// Removes a runtime tag from a slot or an object. Returns whether anything changed.
pub fn remove_runtime_tag(world: &mut World, target: Entity, tag: Tag) -> bool {
    let changed = if let Some(mut slot) = world.get_mut::<SmartObjectSlot>(target) {
        slot.runtime_tags.remove(tag)
    } else if let Some(mut object) = world.get_mut::<SmartObject>(target) {
        object.runtime_tags.remove(tag)
    } else {
        false
    };
    if changed {
        emit_changed(world, target, SlotChange::TagRemoved, Some(tag));
    }
    changed
}

/// The default reason `set_enabled` uses.
pub const DEFAULT_OFF_REASON: &str = "gameplay";

/// Switches `target` (a slot or an object) on or off for `reason`. A target keeps a set of
/// off-reasons and is enabled only while its `enabled` flag is set and the set is empty, so two
/// disables need two enables. Returns the target's own effective value afterwards (`None` when
/// `target` is neither a slot nor an object) and queues `Enabled` / `Disabled` when it changed.
pub fn set_enabled(world: &mut World, target: Entity, enabled: bool, reason: &str) -> Option<bool> {
    let (before, after) = if let Some(mut slot) = world.get_mut::<SmartObjectSlot>(target) {
        let before = slot.enabled && slot.off_reasons.is_empty();
        if enabled {
            slot.off_reasons.remove(reason);
        } else {
            slot.off_reasons.insert(reason.to_string());
        }
        (before, slot.enabled && slot.off_reasons.is_empty())
    } else {
        let mut object = world.get_mut::<SmartObject>(target)?;
        let before = object.enabled && object.off_reasons.is_empty();
        if enabled {
            object.off_reasons.remove(reason);
        } else {
            object.off_reasons.insert(reason.to_string());
        }
        (before, object.enabled && object.off_reasons.is_empty())
    };
    if before != after {
        let what = if after {
            SlotChange::Enabled
        } else {
            SlotChange::Disabled
        };
        emit_changed(world, target, what, None);
    }
    Some(after)
}

/// Queues a `Custom` change for `slot` carrying `tag`.
pub fn notify_slot(world: &mut World, slot: Entity, tag: Tag) {
    emit_changed(world, slot, SlotChange::Custom, Some(tag));
}

// ---- definition-derived components -------------------------------------------------------

/// How a definition's activity tags and a slot's own combine (spec 39 § 5.2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ActivityMerge {
    /// Both sets together.
    #[default]
    Union,
    /// The slot's own tags if it has any, else the definition's.
    SlotFirst,
}

/// Which requester rules apply (spec 39 § 5.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UserRule {
    /// The definition's and the slot's must both pass.
    #[default]
    DefinitionAndSlot,
    /// Only the slot's.
    SlotOnly,
    /// The slot's if present, otherwise the definition's.
    SlotElseDefinition,
}

/// The object-level search rules (the shared part of a definition). An object without this
/// component has no activity tags, merges as `Union`, and has no requester rule.
#[derive(Component, Debug, Clone, Default, PartialEq)]
pub struct SmartObjectRules {
    /// Activity tags every slot offers.
    pub activity: TagContainer,
    /// How they combine with a slot's own.
    pub activity_merge: ActivityMerge,
    /// Which requester rules apply.
    pub user_rule: UserRule,
    /// The object-level requester rule.
    pub user_query: Option<TagQuery>,
}

/// A named [`SlotCondition`] with its parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConditionRef {
    /// The registered type path.
    pub path: String,
    /// String parameters handed to the condition.
    pub params: BTreeMap<String, String>,
}

/// Conditions on an object (all its slots) or on one slot; evaluated only by search with
/// `check_conditions`.
#[derive(Component, Debug, Clone, Default, PartialEq, Eq)]
pub struct SmartObjectConditions(pub Vec<ConditionRef>);

/// Behavior payloads a slot offers: family name to RON text, decoded on demand by
/// [`slot_behavior`].
#[derive(Component, Debug, Clone, Default, PartialEq, Eq)]
pub struct SlotBehaviors(pub BTreeMap<String, String>);

/// What a [`SlotCondition`] sees.
pub struct ConditionCtx<'a> {
    /// The world.
    pub world: &'a World,
    /// The slot's owning object, when it has one.
    pub object: Option<Entity>,
    /// The slot being tested.
    pub slot: Entity,
    /// The requesting entity ([`SlotQuery::user`]).
    pub user: Option<Entity>,
    /// The parameters of the [`ConditionRef`].
    pub params: &'a BTreeMap<String, String>,
}

/// A project-defined precondition of a slot, registered by type path in the
/// [`SmartObjectRegistry`].
pub trait SlotCondition: Send + Sync + 'static {
    /// Whether the slot passes right now.
    fn test(&self, ctx: &ConditionCtx<'_>) -> bool;
}

type Decoder = Box<dyn Fn(&str) -> Option<Box<dyn Any + Send + Sync>> + Send + Sync>;

struct BehaviorType {
    type_id: TypeId,
    decode: Decoder,
}

struct CachedBehavior {
    text: String,
    value: Option<Box<dyn Any + Send + Sync>>,
}

/// Registered conditions and behavior types, plus the per-slot decode cache and the warn-once set.
#[derive(Resource, Default)]
pub struct SmartObjectRegistry {
    conditions: BTreeMap<String, Box<dyn SlotCondition>>,
    behaviors: BTreeMap<String, BehaviorType>,
    cache: Mutex<BTreeMap<(u64, String), CachedBehavior>>,
    warned: Mutex<BTreeSet<String>>,
}

impl SmartObjectRegistry {
    /// Registers a condition under its type `path`.
    pub fn register_condition(&mut self, path: impl Into<String>, condition: impl SlotCondition) {
        self.conditions.insert(path.into(), Box::new(condition));
    }

    /// Registers the type a behavior `family` decodes into.
    pub fn register_behavior<T: DeserializeOwned + Clone + Send + Sync + 'static>(
        &mut self,
        family: impl Into<String>,
    ) {
        self.behaviors.insert(
            family.into(),
            BehaviorType {
                type_id: TypeId::of::<T>(),
                decode: Box::new(|text| {
                    ron::from_str::<T>(text)
                        .ok()
                        .map(|v| Box::new(v) as Box<dyn Any + Send + Sync>)
                }),
            },
        );
    }

    /// Whether a condition is registered under `path`.
    pub fn has_condition(&self, path: &str) -> bool {
        self.conditions.contains_key(path)
    }

    /// Returns `true` the first time it is called with `key`.
    fn warn_once(&self, key: String) -> bool {
        self.warned.lock().map(|mut set| set.insert(key)).unwrap_or(false)
    }
}

/// Decodes the payload of `family` on `slot` into the type registered for it, caching the result
/// per slot. An unknown family, a type mismatch or a decode failure answers `None` and logs once
/// per slot and family.
pub fn slot_behavior<T: Clone + 'static>(world: &World, slot: Entity, family: &str) -> Option<T> {
    let registry = world.get_resource::<SmartObjectRegistry>()?;
    let key = (slot.to_bits(), family.to_string());
    let text = world.get::<SlotBehaviors>(slot)?.0.get(family)?;
    let Some(entry) = registry
        .behaviors
        .get(family)
        .filter(|b| b.type_id == TypeId::of::<T>())
    else {
        if registry.warn_once(format!("type:{}:{family}", slot.to_bits())) {
            tracing::warn!(target: "regia_ai", family, "smart-object behavior family has no matching registered type");
        }
        return None;
    };
    let mut cache = registry.cache.lock().ok()?;
    let fresh = cache.get(&key).is_none_or(|c| &c.text != text);
    if fresh {
        let value = (entry.decode)(text);
        if value.is_none() && registry.warn_once(format!("decode:{}:{family}", slot.to_bits())) {
            tracing::warn!(target: "regia_ai", family, "smart-object behavior payload failed to decode");
        }
        cache.insert(
            key.clone(),
            CachedBehavior {
                text: text.clone(),
                value,
            },
        );
    }
    cache.get(&key)?.value.as_ref()?.downcast_ref::<T>().cloned()
}

/// `PostUpdate`: drops behavior cache entries (and their warn-once keys) whose slot entity is gone
/// or no longer carries that behavior family, so streaming levels in and out or reloading
/// definitions does not grow the cache without bound (audit-gameplay zai L3, ADR-1809).
pub(crate) fn sweep_behavior_cache(world: &mut World) {
    let Some(registry) = world.get_resource::<SmartObjectRegistry>() else {
        return;
    };
    let Ok(mut cache) = registry.cache.lock() else {
        return;
    };
    if cache.is_empty() {
        return;
    }
    let _span =
        tracing::trace_span!(target: "regia_ai", "smart_object_cache_sweep", entries = cache.len()).entered();
    let live = |bits: u64, family: &str| {
        Entity::try_from_bits(bits)
            .and_then(|slot| world.get::<SlotBehaviors>(slot))
            .is_some_and(|behaviors| behaviors.0.contains_key(family))
    };
    let mut gone: BTreeSet<u64> = BTreeSet::new();
    cache.retain(|(bits, family), _| {
        let keep = live(*bits, family);
        if !keep {
            gone.insert(*bits);
        }
        keep
    });
    drop(cache);
    if !gone.is_empty()
        && let Ok(mut warned) = registry.warned.lock()
    {
        warned.retain(|key| {
            let bits = key.split(':').nth(1).and_then(|b| b.parse::<u64>().ok());
            !bits.is_some_and(|b| gone.contains(&b))
        });
    }
}

/// The number of cached behavior decodes (for tests and debug views).
#[must_use]
pub fn behavior_cache_len(world: &World) -> usize {
    world
        .get_resource::<SmartObjectRegistry>()
        .and_then(|registry| registry.cache.lock().ok().map(|cache| cache.len()))
        .unwrap_or(0)
}

// ---- search ------------------------------------------------------------------------------

/// Where [`SmartObjectIndex::search`] looks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum QueryRegion {
    /// A ball.
    Sphere {
        /// Center.
        center: Vec3,
        /// Radius (non-negative, finite).
        radius: f32,
    },
    /// A box.
    Aabb {
        /// Minimum corner.
        min: Vec3,
        /// Maximum corner.
        max: Vec3,
    },
}

impl QueryRegion {
    /// Whether `point` (finite) lies inside the region.
    pub fn contains(&self, point: Vec3) -> bool {
        if !point.is_finite() {
            return false;
        }
        match *self {
            Self::Sphere { center, radius } => (point - center).length() <= radius,
            Self::Aabb { min, max } => point.cmpge(min).all() && point.cmple(max).all(),
        }
    }

    /// The point distances are measured from: the sphere's center or the box's center.
    pub fn reference(&self) -> Vec3 {
        match *self {
            Self::Sphere { center, .. } => center,
            Self::Aabb { min, max } => {
                // Averaged in f64: `min + max` overflows f32 for a box near the type's limit.
                let mid = |a: f32, b: f32| ((f64::from(a) + f64::from(b)) * 0.5) as f32;
                Vec3::new(mid(min.x, max.x), mid(min.y, max.y), mid(min.z, max.z))
            }
        }
    }

    /// Whether the region's numbers are finite and non-negative where required.
    pub fn is_valid(&self) -> bool {
        match *self {
            Self::Sphere { center, radius } => center.is_finite() && radius.is_finite() && radius >= 0.0,
            Self::Aabb { min, max } => min.is_finite() && max.is_finite() && min.cmple(max).all(),
        }
    }
}

/// A slot search (spec 39 § 5).
#[derive(Debug, Clone)]
pub struct SlotQuery {
    /// Where to look.
    pub region: QueryRegion,
    /// Tested against the slot's effective activity set; `None` passes.
    pub activity: Option<TagQuery>,
    /// The requester's tags (tested against the user rules).
    pub user_tags: TagContainer,
    /// When non-empty, the slot must offer one of these behavior families.
    pub families: Vec<String>,
    /// The requester's claim priority.
    pub priority: ClaimPriority,
    /// Also list slots claimed at a lower priority.
    pub allow_preempt: bool,
    /// Also list disabled slots.
    pub include_disabled: bool,
    /// Run the object and slot conditions (default true).
    pub check_conditions: bool,
    /// The requesting entity; a slot it already holds counts as free for it.
    pub user: Option<Entity>,
    /// Keep at most this many hits (after sorting).
    pub limit: Option<usize>,
}

impl SlotQuery {
    /// A query over `region` with the defaults of the spec.
    pub fn new(region: QueryRegion) -> Self {
        Self {
            region,
            activity: None,
            user_tags: TagContainer::new(),
            families: Vec::new(),
            priority: ClaimPriority::Normal,
            allow_preempt: false,
            include_disabled: false,
            check_conditions: true,
            user: None,
            limit: None,
        }
    }
}

/// How a hit can be claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitStatus {
    /// Claimable now.
    Free,
    /// Held at a lower priority than the query's.
    Preemptable {
        /// The current holder.
        holder: Entity,
    },
}

/// One search result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlotHit {
    /// The slot.
    pub slot: Entity,
    /// Its owning object.
    pub object: Option<Entity>,
    /// Distance from the region's reference point to the slot.
    pub distance: f32,
    /// How it can be claimed.
    pub status: HitStatus,
}

/// Why a claim at some priority could not succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailableWhy {
    /// The slot or its object is disabled.
    Disabled,
    /// Held at an equal or higher priority.
    HeldHigher,
    /// In use.
    InUse,
    /// Not a slot.
    NotFound,
}

/// Whether a claim at some priority could succeed (conditions and tag filters ignored).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// Free.
    Free,
    /// Held at a lower priority.
    Preemptable,
    /// No.
    Unavailable(UnavailableWhy),
}

/// The search stage that rejected a candidate (stages run in this order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectStage {
    /// Outside the region.
    Region,
    /// Disabled.
    Enabled,
    /// Claimed (and not preemptable) or in use.
    State,
    /// No wanted behavior family.
    Family,
    /// The activity query failed.
    Activity,
    /// The requester rule failed.
    User,
    /// A condition failed.
    Condition,
}

impl RejectStage {
    /// The lowercase name used by the agent protocol.
    pub fn name(self) -> &'static str {
        match self {
            Self::Region => "region",
            Self::Enabled => "enabled",
            Self::State => "state",
            Self::Family => "family",
            Self::Activity => "activity",
            Self::User => "user",
            Self::Condition => "condition",
        }
    }
}

/// One rejected candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// The owning object.
    pub object: Option<Entity>,
    /// The slot, or `None` when an object-level check rejected every slot of the object alike.
    pub slot: Option<Entity>,
    /// The first failing stage.
    pub stage: RejectStage,
    /// A short explanation.
    pub detail: String,
}

/// Collects the rejections of a search (the `explain` mode).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RejectionLog {
    /// The rejections, in index order.
    pub entries: Vec<Rejection>,
}

struct Reject {
    stage: RejectStage,
    detail: String,
    /// The check looked only at the object, so every slot of it fails the same way.
    object_level: bool,
}

fn reject(stage: RejectStage, detail: &str, object_level: bool, detailed: bool) -> Reject {
    Reject {
        stage,
        detail: if detailed {
            detail.to_string()
        } else {
            String::new()
        },
        object_level,
    }
}

/// The containers a slot's activity set is the union of (spec 39 § 5.2), as a [`TagLookup`], so a
/// query over it builds nothing.
struct ActivityTags<'a>([Option<&'a TagContainer>; 4]);

impl ActivityTags<'_> {
    fn sources(&self) -> impl Iterator<Item = &TagContainer> {
        self.0.iter().flatten().copied()
    }
}

impl TagLookup for ActivityTags<'_> {
    fn has_tag(&self, tag: Tag) -> bool {
        self.sources().any(|c| c.has(tag))
    }

    fn has_tag_exact(&self, tag: Tag) -> bool {
        self.sources().any(|c| c.has_exact(tag))
    }
}

fn activity_tags<'a>(world: &'a World, info: &SlotInfo<'_>, slot: &'a SmartObjectSlot) -> ActivityTags<'a> {
    let object = (info.object != Entity::PLACEHOLDER).then_some(info.object);
    let rules = object.and_then(|o| world.get::<SmartObjectRules>(o));
    let defined = rules.map(|r| &r.activity);
    let own = Some(&slot.activity);
    let (first, second) = match rules.map_or(ActivityMerge::Union, |r| r.activity_merge) {
        ActivityMerge::Union => (defined, own),
        ActivityMerge::SlotFirst if !slot.activity.is_empty() => (own, None),
        ActivityMerge::SlotFirst => (defined, None),
    };
    let object_runtime = object
        .and_then(|o| world.get::<SmartObject>(o))
        .map(|component| &component.runtime_tags);
    ActivityTags([first, second, Some(&slot.runtime_tags), object_runtime])
}

fn user_rule_passes(
    world: &World,
    object: Option<Entity>,
    slot: &SmartObjectSlot,
    tags: &TagContainer,
) -> Result<(), bool> {
    let rules = object.and_then(|o| world.get::<SmartObjectRules>(o));
    let rule = rules.map_or(UserRule::DefinitionAndSlot, |r| r.user_rule);
    let object_query = rules.and_then(|r| r.user_query.as_ref());
    let slot_query = slot.user_query.as_ref();
    let pass = |q: Option<&TagQuery>| q.is_none_or(|q| tags.matches(q));
    let (object_ok, slot_ok) = match rule {
        UserRule::DefinitionAndSlot => (pass(object_query), pass(slot_query)),
        UserRule::SlotOnly => (true, pass(slot_query)),
        UserRule::SlotElseDefinition => match slot_query {
            Some(_) => (true, pass(slot_query)),
            None => (pass(object_query), true),
        },
    };
    if object_ok && slot_ok {
        Ok(())
    } else {
        // Object-level when only the object's rule failed.
        Err(!object_ok && slot_ok)
    }
}

fn conditions_pass(
    world: &World,
    object: Option<Entity>,
    slot: Entity,
    user: Option<Entity>,
    detailed: bool,
) -> Result<(), Reject> {
    let registry = world.get_resource::<SmartObjectRegistry>();
    let run = |refs: &SmartObjectConditions, object_level: bool| -> Result<(), Reject> {
        for condition in &refs.0 {
            let passed = match registry.and_then(|r| r.conditions.get(&condition.path)) {
                Some(c) => c.test(&ConditionCtx {
                    world,
                    object,
                    slot,
                    user,
                    params: &condition.params,
                }),
                None => {
                    if let Some(r) = registry
                        && r.warn_once(format!("condition_not_registered:{}", condition.path))
                    {
                        tracing::warn!(target: "regia_ai", path = %condition.path, "condition_not_registered");
                    }
                    false
                }
            };
            if !passed {
                return Err(Reject {
                    stage: RejectStage::Condition,
                    detail: if detailed {
                        format!("condition {} failed", condition.path)
                    } else {
                        String::new()
                    },
                    object_level,
                });
            }
        }
        Ok(())
    };
    if let Some(refs) = object.and_then(|o| world.get::<SmartObjectConditions>(o)) {
        run(refs, true)?;
    }
    if let Some(refs) = world.get::<SmartObjectConditions>(slot) {
        run(refs, false)?;
    }
    Ok(())
}

fn evaluate(world: &World, info: &SlotInfo<'_>, q: &SlotQuery, detailed: bool) -> Result<HitStatus, Reject> {
    if !q.region.contains(info.position) {
        return Err(reject(RejectStage::Region, "outside the region", false, detailed));
    }
    let Some(slot) = world.get::<SmartObjectSlot>(info.entity) else {
        return Err(reject(RejectStage::State, "not a slot", false, detailed));
    };
    let object = (info.object != Entity::PLACEHOLDER).then_some(info.object);
    let disabled = !effective_enabled(world, info.entity);
    if disabled && !q.include_disabled {
        let object_off = object
            .and_then(|o| world.get::<SmartObject>(o))
            .is_some_and(|o| !o.enabled || !o.off_reasons.is_empty());
        let slot_off = !slot.enabled || !slot.off_reasons.is_empty();
        let detail = if object_off && !slot_off {
            "object disabled"
        } else {
            "slot disabled"
        };
        return Err(reject(
            RejectStage::Enabled,
            detail,
            object_off && !slot_off,
            detailed,
        ));
    }
    let status = match slot.claimed_by {
        Some(holder) if q.user == Some(holder) => HitStatus::Free,
        Some(_) if slot.in_use => return Err(reject(RejectStage::State, "in use", false, detailed)),
        Some(holder) => {
            if q.allow_preempt && q.priority > slot.priority_held {
                HitStatus::Preemptable { holder }
            } else {
                return Err(reject(RejectStage::State, "claimed", false, detailed));
            }
        }
        None => HitStatus::Free,
    };
    if !q.families.is_empty() {
        let offers = world
            .get::<SlotBehaviors>(info.entity)
            .is_some_and(|b| q.families.iter().any(|f| b.0.contains_key(f)));
        if !offers {
            return Err(reject(
                RejectStage::Family,
                "no wanted behavior family",
                false,
                detailed,
            ));
        }
    }
    if let Some(query) = &q.activity
        && !query.eval_on(&activity_tags(world, info, slot))
    {
        return Err(reject(
            RejectStage::Activity,
            "activity query failed",
            false,
            detailed,
        ));
    }
    if let Err(object_level) = user_rule_passes(world, object, slot, &q.user_tags) {
        return Err(reject(
            RejectStage::User,
            "requester rule failed",
            object_level,
            detailed,
        ));
    }
    if q.check_conditions {
        conditions_pass(world, object, info.entity, q.user, detailed)?;
    }
    Ok(status)
}

/// Collapses object-level rejections that hit every slot of an object alike into one `slot: None`
/// entry (placed where the object's first rejection was).
fn collapse_object_rejections(
    entries: Vec<(Rejection, bool)>,
    slots_per_object: &BTreeMap<u64, usize>,
) -> Vec<Rejection> {
    let mut by_object: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (i, (r, object_level)) in entries.iter().enumerate() {
        if *object_level && let Some(o) = r.object {
            by_object.entry(o.to_bits()).or_default().push(i);
        }
    }
    let mut collapsed: BTreeSet<usize> = BTreeSet::new();
    let mut dropped: BTreeSet<usize> = BTreeSet::new();
    for (object_bits, indices) in by_object {
        let total = slots_per_object.get(&object_bits).copied().unwrap_or(usize::MAX);
        let first = &entries[indices[0]].0;
        let alike = indices
            .iter()
            .all(|&i| entries[i].0.stage == first.stage && entries[i].0.detail == first.detail);
        if indices.len() == total && alike {
            collapsed.insert(indices[0]);
            dropped.extend(indices[1..].iter().copied());
        }
    }
    let mut out = Vec::with_capacity(entries.len());
    for (i, (mut r, _)) in entries.into_iter().enumerate() {
        if dropped.contains(&i) {
            continue;
        }
        if collapsed.contains(&i) {
            r.slot = None;
        }
        out.push(r);
    }
    out
}

/// The distance between two finite points: the plain f32 length, or, when that overflows to
/// infinity (a pair 1e19 m or more apart), the f64 length, so far slots still order by distance.
fn distance_between(a: Vec3, b: Vec3) -> f32 {
    let plain = (a - b).length();
    if plain.is_finite() {
        return plain;
    }
    let d = |x: f32, y: f32| f64::from(x) - f64::from(y);
    let (dx, dy, dz) = (d(a.x, b.x), d(a.y, b.y), d(a.z, b.z));
    (dx * dx + dy * dy + dz * dz).sqrt() as f32
}

impl SmartObjectIndex {
    /// Slots matching `q`, sorted by `(distance, the slot's order in the index)`; `limit` applies
    /// after sorting. The order-in-the-index tie-break is the slot's `(role, arrival)` ordinal,
    /// never the entity id and never the order the candidates happened to be visited in (the
    /// plain path walks grid cells, the logged path walks the whole list), so the results are
    /// the same with or without a [`RejectionLog`].
    #[tracing::instrument(skip_all, target = "regia_ai", name = "smart_object_search")]
    pub fn search(&self, world: &World, q: &SlotQuery, log: Option<&mut RejectionLog>) -> Vec<SlotHit> {
        let reference = q.region.reference();
        let detailed = log.is_some();
        let mut hits: Vec<(u32, SlotHit)> = Vec::new();
        let mut rejected: Vec<(Rejection, bool)> = Vec::new();
        let mut slots_per_object: BTreeMap<u64, usize> = BTreeMap::new();
        let mut visit = |info: SlotInfo<'_>, ordinal: u32| {
            let object = (info.object != Entity::PLACEHOLDER).then_some(info.object);
            if !info.position.is_finite() {
                return;
            }
            // Only slots that reach evaluation count: a skipped one never yields a rejection, so
            // counting it would stop an object-level failure from collapsing to one entry.
            if detailed && let Some(o) = object {
                *slots_per_object.entry(o.to_bits()).or_default() += 1;
            }
            match evaluate(world, &info, q, detailed) {
                Ok(status) => hits.push((
                    ordinal,
                    SlotHit {
                        slot: info.entity,
                        object,
                        distance: distance_between(info.position, reference),
                        status,
                    },
                )),
                Err(r) if detailed => rejected.push((
                    Rejection {
                        object,
                        slot: Some(info.entity),
                        stage: r.stage,
                        detail: r.detail,
                    },
                    r.object_level,
                )),
                Err(_) => {}
            }
        };
        // A rejection log wants every slot looked at (region rejections included), so only a
        // plain search uses the grid. Both paths must still agree on the result: the sort key is
        // `(distance, ordinal)`, a total order independent of the visitation order and of the
        // entity id (ADR-4651).
        if detailed {
            self.slots()
                .enumerate()
                .for_each(|(i, info)| visit(info, i as u32));
        } else {
            self.for_each_candidate(&q.region, &mut visit);
        }
        hits.sort_by(|a, b| a.1.distance.total_cmp(&b.1.distance).then_with(|| a.0.cmp(&b.0)));
        let mut hits: Vec<SlotHit> = hits.into_iter().map(|(_, hit)| hit).collect();
        if let Some(limit) = q.limit {
            hits.truncate(limit);
        }
        if let Some(log) = log {
            log.entries = collapse_object_rejections(rejected, &slots_per_object);
        }
        hits
    }

    /// The nearest hit of `q`, if any.
    pub fn find_first(&self, world: &World, q: &SlotQuery) -> Option<SlotHit> {
        let mut q = q.clone();
        q.limit = Some(1);
        self.search(world, &q, None).into_iter().next()
    }

    /// Whether a claim on `slot` at `priority` could succeed (conditions and tag filters ignored).
    ///
    /// This is the view of a caller that does not hold the slot: it takes no user, so a slot in
    /// use reports `InUse` even to its own holder, for whom `search` with `SlotQuery::user` set
    /// (and `claim_with`) treat it as free. A holder asks `search` (audit-gameplay-muse L7).
    pub fn availability(&self, world: &World, slot: Entity, priority: ClaimPriority) -> Availability {
        let Some(state) = world.get::<SmartObjectSlot>(slot) else {
            return Availability::Unavailable(UnavailableWhy::NotFound);
        };
        if !effective_enabled(world, slot) {
            return Availability::Unavailable(UnavailableWhy::Disabled);
        }
        match state.claimed_by {
            None => Availability::Free,
            Some(_) if state.in_use => Availability::Unavailable(UnavailableWhy::InUse),
            Some(_) if priority > state.priority_held => Availability::Preemptable,
            Some(_) => Availability::Unavailable(UnavailableWhy::HeldHigher),
        }
    }
}

// ---- entrances and transforms ------------------------------------------------------------

/// Which way an entrance is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntranceKind {
    /// Used to reach the slot.
    Enter,
    /// Used to leave it.
    Exit,
    /// Either.
    Both,
}

/// An approach point of a slot: a component on a child of the slot (its `GlobalTransform` is the
/// point).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotEntrance {
    /// Which way it is used.
    pub kind: EntranceKind,
    /// Higher is preferred.
    pub rank: i8,
}

/// The slot's world transform, if it has been propagated.
pub fn slot_transform(world: &World, slot: Entity) -> Option<GlobalTransform> {
    world.get::<GlobalTransform>(slot).copied()
}

/// The entrance of `slot` to use from `from`: the highest `rank` among entrances matching `kind`
/// (an entrance of kind `Both` matches either), ties by distance to `from`, then entity bits. With
/// `nav` (a navigation query and its half-extents) the point is projected onto the navigation mesh
/// and an entrance that cannot be projected is skipped.
pub fn entrance_for(
    world: &World,
    slot: Entity,
    from: Vec3,
    kind: EntranceKind,
    nav: Option<(&NavQuery<'_>, Vec3)>,
) -> Option<Vec3> {
    let children = world.get::<Children>(slot)?;
    let mut best: Option<(i8, f32, u64, Vec3)> = None;
    for child in children.iter() {
        let (Some(entrance), Some(transform)) = (
            world.get::<SlotEntrance>(child),
            world.get::<GlobalTransform>(child),
        ) else {
            continue;
        };
        if !(entrance.kind == kind || entrance.kind == EntranceKind::Both || kind == EntranceKind::Both) {
            continue;
        }
        let mut point = transform.translation();
        if let Some((query, extents)) = nav {
            match query.find_nearest(point, extents) {
                Some((_, projected)) => point = projected,
                None => continue,
            }
        }
        let candidate = (entrance.rank, (point - from).length(), child.to_bits(), point);
        let better = best.is_none_or(|b| {
            candidate
                .0
                .cmp(&b.0)
                .then(b.1.total_cmp(&candidate.1))
                .then(b.2.cmp(&candidate.2))
                .is_gt()
        });
        if better {
            best = Some(candidate);
        }
    }
    best.map(|b| b.3)
}

// ---- plugin wiring -----------------------------------------------------------------------

/// Registers the v2 resources and messages (called by [`crate::SmartObjectPlugin`]).
pub(crate) fn build(app: &mut App) {
    app.init_resource::<SmartObjectEvents>()
        .init_resource::<ClaimLedger>()
        .init_resource::<SmartObjectRegistry>()
        .add_message::<SlotChanged>()
        .add_message::<SlotLost>();
}
