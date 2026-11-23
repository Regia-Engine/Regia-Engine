// SPDX-License-Identifier: MPL-2.0
//! The smart-object definition asset (`.smartobject.ron`, format version 1;
//! `docs/specs/39-smart-objects-v2.md` § 3, § 4) and the system that materializes it into slot
//! entities.
//!
//! A definition describes the shared part of an object (activity tags, requester rules,
//! conditions, behavior payloads) and its slots (offset, own tags, entrances). An object entity
//! carries [`SmartObjectDefinition`]; `smart_object_materialize` (`PostUpdate`, before transform
//! propagation) spawns one child per definition slot, fills the search components of
//! [`crate::smart_object_v2`], and reconciles by definition slot index when the definition changes:
//! survivors keep their entity id and claim state, removed slots invalidate their holder
//! ([`LostReason::SlotRemoved`]) and despawn, new slots spawn.
//!
//! The definition is plain data parsed with [`SmartObjectDef::from_ron`]; wiring it to the asset
//! server (an `AssetRef` on the object) is left to the host, which hands the parsed
//! `Arc<SmartObjectDef>` to the component.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_framework::tags::{Tag, TagContainer, TagQuery, TagRegistry};
use regia_scene::components::Name;
use regia_scene::{GlobalTransform, Transform};
use serde::{Deserialize, Serialize};

use crate::smart_object::{SmartObject, SmartObjectSlot};
use crate::smart_object_v2::{
    ActivityMerge, ConditionRef, EntranceKind, LostReason, SlotBehaviors, SlotEntrance,
    SmartObjectConditions, SmartObjectRules, UserRule, invalidate_claim,
};

/// Current `.smartobject.ron` format version.
pub const SMART_OBJECT_DEF_VERSION: u32 = 1;

/// Type path of a cooked definition in the asset manifest and the cooked header. The payload is the
/// definition's canonical RON text ([`SmartObjectDef::to_ron`]).
pub const SMART_OBJECT_DEF_TYPE_PATH: &str = "regia_ai::smart_object_def::SmartObjectDef";

/// The most slots one definition may declare.
pub const MAX_DEF_SLOTS: usize = 64;

/// The most entrances one slot may declare.
pub const MAX_DEF_ENTRANCES: usize = 8;

/// A local transform: translation and an xyzw quaternion (normalized at load).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OffsetDef {
    /// Translation in the parent's space.
    #[serde(default)]
    pub translation: (f32, f32, f32),
    /// Rotation as an xyzw quaternion (default identity).
    #[serde(default = "identity_rotation")]
    pub rotation: (f32, f32, f32, f32),
}

impl Default for OffsetDef {
    fn default() -> Self {
        Self {
            translation: (0.0, 0.0, 0.0),
            rotation: identity_rotation(),
        }
    }
}

fn identity_rotation() -> (f32, f32, f32, f32) {
    (0.0, 0.0, 0.0, 1.0)
}

fn truth() -> bool {
    true
}

impl OffsetDef {
    fn transform(&self) -> Transform {
        let (x, y, z) = self.translation;
        let (qx, qy, qz, qw) = self.rotation;
        Transform {
            translation: Vec3::new(x, y, z),
            rotation: Quat::from_xyzw(qx, qy, qz, qw).normalize(),
            ..Transform::IDENTITY
        }
    }

    fn is_valid(&self) -> bool {
        let (x, y, z) = self.translation;
        let (qx, qy, qz, qw) = self.rotation;
        // The squared length must also be finite: a quaternion such as `(1e20, 0, 0, 0)` overflows
        // it, and `normalize` then gives a zero rotation. Regression:
        // tests/tqs_sweep.rs::smart_object_def_validate_rejects_rotations_that_cannot_be_normalized.
        let length_squared = Quat::from_xyzw(qx, qy, qz, qw).length_squared();
        [x, y, z, qx, qy, qz, qw].iter().all(|v| v.is_finite())
            && length_squared.is_finite()
            && length_squared > 1e-12
    }
}

/// How the definition's activity tags and a slot's own combine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivityMergeDef {
    /// Both sets together.
    #[default]
    Union,
    /// The slot's own tags if it has any, else the definition's.
    SlotFirst,
}

/// Which requester rules apply.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserRuleDef {
    /// The definition's and the slot's must both pass.
    #[default]
    DefinitionAndSlot,
    /// Only the slot's.
    SlotOnly,
    /// The slot's if present, otherwise the definition's.
    SlotElseDefinition,
}

/// Which way an entrance is used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntranceKindDef {
    /// Used to reach the slot.
    Enter,
    /// Used to leave it.
    Exit,
    /// Either.
    #[default]
    Both,
}

/// A named precondition with string parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionDef {
    /// The registered `SlotCondition` type path.
    pub path: String,
    /// String parameters handed to the condition.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
}

/// A behavior payload: a consumer-defined family and RON text decoded on demand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehaviorDef {
    /// The family name.
    pub family: String,
    /// The payload as RON text.
    #[serde(default)]
    pub data: String,
}

/// An approach point of a slot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntranceDef {
    /// Where, relative to the slot.
    #[serde(default)]
    pub offset: OffsetDef,
    /// Which way it is used (default both).
    #[serde(default)]
    pub kind: EntranceKindDef,
    /// Higher is preferred (default 0).
    #[serde(default)]
    pub rank: i8,
}

/// One slot of a definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlotDef {
    /// Unique name, `[A-Za-z0-9_]{1,32}`; the slot's role.
    pub name: String,
    /// Local transform in the object's space.
    #[serde(default)]
    pub offset: OffsetDef,
    /// Activity tags of this slot.
    #[serde(default)]
    pub activity: Vec<String>,
    /// Runtime tags a new slot starts with.
    #[serde(default)]
    pub initial_tags: Vec<String>,
    /// Whether the slot starts enabled (default true).
    #[serde(default = "truth")]
    pub enabled: bool,
    /// The slot's own requester rule.
    #[serde(default)]
    pub user_query: Option<TagQuery>,
    /// Slot-level conditions.
    #[serde(default)]
    pub conditions: Vec<ConditionDef>,
    /// Behavior payloads; for a family the definition also lists, this entry wins.
    #[serde(default)]
    pub behaviors: Vec<BehaviorDef>,
    /// Approach points.
    #[serde(default)]
    pub entrances: Vec<EntranceDef>,
}

/// The `.smartobject.ron` document.
///
/// ```ron
/// SmartObjectDef(
///     format_version: 1,
///     activity: ["Activity.Sit"],
///     slots: [
///         (name: "left", offset: (translation: (-0.4, 0.0, 0.0))),
///         (name: "right", offset: (translation: (0.4, 0.0, 0.0))),
///     ],
/// )
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TypePath)]
pub struct SmartObjectDef {
    /// Format version, [`SMART_OBJECT_DEF_VERSION`].
    pub format_version: u32,
    /// Activity tags offered by every slot.
    #[serde(default)]
    pub activity: Vec<String>,
    /// How they combine with a slot's own tags.
    #[serde(default)]
    pub activity_merge: ActivityMergeDef,
    /// Which requester rules apply.
    #[serde(default)]
    pub user_rule: UserRuleDef,
    /// The definition-level requester rule.
    #[serde(default)]
    pub user_query: Option<TagQuery>,
    /// Object-level conditions.
    #[serde(default)]
    pub conditions: Vec<ConditionDef>,
    /// Behavior payloads every slot inherits.
    #[serde(default)]
    pub behaviors: Vec<BehaviorDef>,
    /// The slots.
    pub slots: Vec<SlotDef>,
}

/// Why a definition could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SmartObjectDefError {
    /// The RON text did not parse.
    #[error("smart object definition RON is invalid: {0}")]
    Parse(String),
    /// The file's `format_version` is not one this build reads.
    #[error(
        "smart object definition format_version {found} is not supported (this build reads version {supported})"
    )]
    UnsupportedVersion {
        /// Version found in the file.
        found: u32,
        /// Version this build reads.
        supported: u32,
    },
    /// A rule of § 3 was broken (the text says which).
    #[error("smart object definition is invalid: {0}")]
    BadDefinition(String),
    /// The definition could not be written as RON (a serializer failure, not a bad file).
    #[error("smart object definition could not be written: {0}")]
    Write(String),
}

impl SmartObjectDefError {
    /// The short snake_case reason the agent protocol reports.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Parse(_) | Self::BadDefinition(_) | Self::Write(_) => "bad_definition",
            Self::UnsupportedVersion { .. } => "unsupported_version",
        }
    }
}

fn valid_slot_name(name: &str) -> bool {
    (1..=32).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

impl SmartObjectDef {
    /// An empty definition.
    pub fn new() -> Self {
        Self {
            format_version: SMART_OBJECT_DEF_VERSION,
            activity: Vec::new(),
            activity_merge: ActivityMergeDef::Union,
            user_rule: UserRuleDef::DefinitionAndSlot,
            user_query: None,
            conditions: Vec::new(),
            behaviors: Vec::new(),
            slots: Vec::new(),
        }
    }

    /// Parses and validates RON text. The version is read first, so a newer file is
    /// [`SmartObjectDefError::UnsupportedVersion`] whatever its body looks like.
    pub fn from_ron(text: &str) -> Result<Self, SmartObjectDefError> {
        let found = regia_core::version::peek_ron_version(text).map_err(SmartObjectDefError::Parse)?;
        if found != SMART_OBJECT_DEF_VERSION {
            return Err(SmartObjectDefError::UnsupportedVersion {
                found,
                supported: SMART_OBJECT_DEF_VERSION,
            });
        }
        let def: Self = ron::from_str(text).map_err(|e| SmartObjectDefError::Parse(e.to_string()))?;
        def.validate()?;
        Ok(def)
    }

    /// Writes the definition as pretty RON.
    pub fn to_ron(&self) -> Result<String, SmartObjectDefError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| SmartObjectDefError::Write(e.to_string()))
    }

    /// Checks the rules of spec 39 § 3: unique slot names of `[A-Za-z0-9_]{1,32}`, at most
    /// [`MAX_DEF_SLOTS`] slots and [`MAX_DEF_ENTRANCES`] entrances per slot, finite offsets with a
    /// non-zero rotation.
    pub fn validate(&self) -> Result<(), SmartObjectDefError> {
        let bad = |text: String| Err(SmartObjectDefError::BadDefinition(text));
        if self.slots.len() > MAX_DEF_SLOTS {
            return bad(format!(
                "{} slots (the limit is {MAX_DEF_SLOTS})",
                self.slots.len()
            ));
        }
        if let Some(query) = &self.user_query
            && let Err(e) = query.validate()
        {
            return bad(format!("user_query: {e}"));
        }
        let mut names = BTreeSet::new();
        for slot in &self.slots {
            if let Some(query) = &slot.user_query
                && let Err(e) = query.validate()
            {
                return bad(format!("slot {:?} user_query: {e}", slot.name));
            }
            if !valid_slot_name(&slot.name) {
                return bad(format!("slot name {:?} is not [A-Za-z0-9_]{{1,32}}", slot.name));
            }
            if !names.insert(slot.name.as_str()) {
                return bad(format!("slot name {:?} is used twice", slot.name));
            }
            if slot.entrances.len() > MAX_DEF_ENTRANCES {
                return bad(format!(
                    "slot {:?} has {} entrances (the limit is {MAX_DEF_ENTRANCES})",
                    slot.name,
                    slot.entrances.len()
                ));
            }
            let offsets_ok = slot.offset.is_valid() && slot.entrances.iter().all(|e| e.offset.is_valid());
            if !offsets_ok {
                return bad(format!(
                    "slot {:?} has a non-finite or zero-rotation offset",
                    slot.name
                ));
            }
        }
        Ok(())
    }
}

impl Default for SmartObjectDef {
    fn default() -> Self {
        Self::new()
    }
}

// ---- runtime ----------------------------------------------------------------------------

/// The definition an object takes its slots from. Set by the host (or the scene loader) after
/// parsing; replacing the `Arc` makes the next `PostUpdate` reconcile the slots.
#[derive(Component, Debug, Clone)]
pub struct SmartObjectDefinition(pub Arc<SmartObjectDef>);

/// The definition last applied to an object (compared by pointer).
#[derive(Component, Debug, Clone)]
pub(crate) struct AppliedDefinition(Arc<SmartObjectDef>);

/// Marks an entrance entity spawned from a definition (so a reload can replace it).
#[derive(Component, Debug, Clone, Copy)]
struct DefinitionEntrance;

/// A tag by name, or `None` (with a warning) when it is invalid or not declared in the loaded
/// project tag table.
fn declared_tag(registry: Option<&TagRegistry>, name: &str) -> Option<Tag> {
    let tag = match Tag::new(name) {
        Ok(tag) => tag,
        Err(error) => {
            tracing::warn!(target: "regia_ai", tag = name, %error, "tag_unknown: not a valid tag name; dropped");
            return None;
        }
    };
    if let Some(registry) = registry
        && !registry.is_empty()
        && registry.get(name).is_none()
    {
        tracing::warn!(target: "regia_ai", tag = name, "tag_unknown: not declared in the tag table; dropped");
        return None;
    }
    Some(tag)
}

fn declared_tags(registry: Option<&TagRegistry>, names: &[String]) -> TagContainer {
    names.iter().filter_map(|n| declared_tag(registry, n)).collect()
}

fn conditions_of(defs: &[ConditionDef]) -> SmartObjectConditions {
    SmartObjectConditions(
        defs.iter()
            .map(|c| ConditionRef {
                path: c.path.clone(),
                params: c.params.clone(),
            })
            .collect(),
    )
}

/// Definition behaviors first, then the slot's: for a family on both, the slot's entry wins.
fn merged_behaviors(def: &SmartObjectDef, slot: &SlotDef) -> SlotBehaviors {
    let mut map = BTreeMap::new();
    for b in def.behaviors.iter().chain(&slot.behaviors) {
        map.insert(b.family.clone(), b.data.clone());
    }
    SlotBehaviors(map)
}

fn entrance_kind(kind: EntranceKindDef) -> EntranceKind {
    match kind {
        EntranceKindDef::Enter => EntranceKind::Enter,
        EntranceKindDef::Exit => EntranceKind::Exit,
        EntranceKindDef::Both => EntranceKind::Both,
    }
}

fn spawn_entrances(world: &mut World, slot: Entity, entrances: &[EntranceDef]) {
    for entrance in entrances {
        world.spawn((
            SlotEntrance {
                kind: entrance_kind(entrance.kind),
                rank: entrance.rank,
            },
            DefinitionEntrance,
            entrance.offset.transform(),
            GlobalTransform::default(),
            ChildOf(slot),
        ));
    }
}

/// Spawns or updates the slot children of `object` to match `def` (see the module docs). The
/// object keeps any `SmartObject` it has and gets one when it has none.
pub fn materialize_object(world: &mut World, object: Entity, def: &SmartObjectDef) {
    let registry = world.get_resource::<TagRegistry>().cloned();
    let registry = registry.as_ref();
    if world.get::<SmartObject>(object).is_none() {
        world.entity_mut(object).insert(SmartObject::default());
    }
    let object_name = world.get::<Name>(object).map_or_else(
        || format!("object{}", object.to_bits()),
        |n| n.as_str().to_string(),
    );
    world.entity_mut(object).insert((
        SmartObjectRules {
            activity: declared_tags(registry, &def.activity),
            activity_merge: match def.activity_merge {
                ActivityMergeDef::Union => ActivityMerge::Union,
                ActivityMergeDef::SlotFirst => ActivityMerge::SlotFirst,
            },
            user_rule: match def.user_rule {
                UserRuleDef::DefinitionAndSlot => UserRule::DefinitionAndSlot,
                UserRuleDef::SlotOnly => UserRule::SlotOnly,
                UserRuleDef::SlotElseDefinition => UserRule::SlotElseDefinition,
            },
            user_query: def.user_query.clone(),
        },
        conditions_of(&def.conditions),
    ));

    // Existing materialized slots by definition index; a second slot with the same index is stale.
    let children: Vec<Entity> = world
        .get::<Children>(object)
        .map(|c| c.iter().collect())
        .unwrap_or_default();
    let mut existing: BTreeMap<u16, Entity> = BTreeMap::new();
    let mut stale: Vec<Entity> = Vec::new();
    for child in children {
        let Some(index) = world.get::<SmartObjectSlot>(child).and_then(|s| s.def_slot) else {
            continue;
        };
        match existing.entry(index) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(child);
            }
            std::collections::btree_map::Entry::Occupied(_) => stale.push(child),
        }
    }

    for (i, slot_def) in def.slots.iter().enumerate() {
        let Ok(index) = u16::try_from(i) else { break };
        let transform = slot_def.offset.transform();
        let activity = declared_tags(registry, &slot_def.activity);
        let behaviors = merged_behaviors(def, slot_def);
        let conditions = conditions_of(&slot_def.conditions);
        match existing.remove(&index) {
            Some(slot) => {
                if let Some(mut state) = world.get_mut::<SmartObjectSlot>(slot) {
                    state.role = slot_def.name.clone();
                    state.enabled = slot_def.enabled;
                    state.activity = activity;
                    state.user_query = slot_def.user_query.clone();
                }
                world.entity_mut(slot).insert((transform, behaviors, conditions));
                let old: Vec<Entity> = world
                    .get::<Children>(slot)
                    .map(|c| {
                        c.iter()
                            .filter(|e| world.get::<DefinitionEntrance>(*e).is_some())
                            .collect()
                    })
                    .unwrap_or_default();
                for entrance in old {
                    world.despawn(entrance);
                }
                spawn_entrances(world, slot, &slot_def.entrances);
            }
            None => {
                let mut state = SmartObjectSlot::new(slot_def.name.clone());
                state.def_slot = Some(index);
                state.enabled = slot_def.enabled;
                state.activity = activity;
                state.user_query = slot_def.user_query.clone();
                state.runtime_tags = declared_tags(registry, &slot_def.initial_tags);
                let slot = world
                    .spawn((
                        state,
                        transform,
                        GlobalTransform::default(),
                        behaviors,
                        conditions,
                        Name::new(format!("{object_name}/slot_{}", slot_def.name)),
                        ChildOf(object),
                    ))
                    .id();
                spawn_entrances(world, slot, &slot_def.entrances);
            }
        }
    }

    // Slots the definition no longer has: invalidate the holder first, then despawn.
    let removed: Vec<Entity> = stale.into_iter().chain(existing.into_values()).collect();
    for slot in removed {
        let claim = world
            .get::<SmartObjectSlot>(slot)
            .and_then(|s| s.claimed_by.map(|holder| (holder, s.generation)));
        if let Some((holder, generation)) = claim {
            invalidate_claim(world, slot, holder, generation, LostReason::SlotRemoved);
        }
        world.despawn(slot);
    }
}

/// `PostUpdate`, before transform propagation and the index rebuild: applies every object
/// definition that is new or was replaced (compared by pointer), in the definitions' arrival
/// order. Entity ids are record-time allocations (ADR-1347): ordering by them would let two runs
/// of the same level that allocated the ids differently spawn the same slots in different
/// arrival orders, which the index's distance tie-breaks would then disagree on.
#[tracing::instrument(skip_all, target = "regia_ai", name = "smart_object_materialize")]
pub(crate) fn smart_object_materialize(
    world: &mut World,
    query: &mut QueryState<(Entity, &SmartObjectDefinition, Option<&AppliedDefinition>)>,
) {
    let _span = tracing::debug_span!("smart_object_materialize").entered();
    let mut todo: Vec<(Entity, Arc<SmartObjectDef>)> = query
        .iter(world)
        .filter(|(_, wanted, applied)| applied.is_none_or(|a| !Arc::ptr_eq(&a.0, &wanted.0)))
        .map(|(entity, wanted, _)| (entity, wanted.0.clone()))
        .collect();
    if todo.is_empty() {
        return;
    }
    // Arrival order; entities the tracking never saw (it started late or could not install its
    // hooks) keep the entity-id fallback, exactly like the slot index's key.
    let key = |world: &World, entity: Entity| {
        world
            .get_resource::<regia_ecs::spawn_order::SpawnOrder<SmartObjectDefinition>>()
            .map_or((u64::MAX, entity.index().index(), entity.to_bits()), |order| {
                order.key(entity)
            })
    };
    todo.sort_by_cached_key(|(entity, _)| key(world, *entity));
    for (object, def) in todo {
        // An earlier object's slot removal may have despawned this one (a child object).
        if world.get_entity(object).is_err() {
            continue;
        }
        materialize_object(world, object, &def);
        world.entity_mut(object).insert(AppliedDefinition(def));
    }
}

/// The `.smartobject.ron` migration chain: version 1 is the first, so there are no steps.
pub fn smart_object_def_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde shape fingerprint of `.smartobject.ron` (hashed by `cargo xtask check-formats`): the
/// field list of every struct reachable from [`SmartObjectDef`], then the spellings of its enums
/// and of the embedded tag query.
pub fn smart_object_def_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<SmartObjectDef>(),
        serde_field_names::<SlotDef>(),
        serde_field_names::<OffsetDef>(),
        serde_field_names::<EntranceDef>(),
        serde_field_names::<ConditionDef>(),
        serde_field_names::<BehaviorDef>(),
        serde_variant_names::<ActivityMergeDef>(),
        serde_variant_names::<UserRuleDef>(),
        serde_variant_names::<EntranceKindDef>(),
        serde_variant_names::<TagQuery>(),
    ])
}

#[cfg(test)]
mod query_limit_tests {
    use super::*;
    use regia_framework::tags::TagQuery;

    fn deep(levels: usize) -> TagQuery {
        let mut q = TagQuery::AllOf(Vec::new());
        for _ in 0..levels {
            q = TagQuery::AllOf(vec![q]);
        }
        q
    }

    #[test]
    fn over_deep_def_level_user_query_is_rejected() {
        let def = SmartObjectDef {
            user_query: Some(deep(20)),
            ..SmartObjectDef::default()
        };
        assert!(matches!(
            def.validate(),
            Err(SmartObjectDefError::BadDefinition(_))
        ));
        let ok = SmartObjectDef {
            user_query: Some(deep(4)),
            ..SmartObjectDef::default()
        };
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn over_deep_slot_user_query_is_rejected_on_load() -> Result<(), Box<dyn std::error::Error>> {
        let mut def = SmartObjectDef::default();
        let slot: SlotDef = ron::from_str("(name: \"a\")")?;
        def.slots.push(SlotDef {
            user_query: Some(deep(20)),
            ..slot
        });
        let text = def.to_ron()?;
        let got = SmartObjectDef::from_ron(&text);
        assert!(
            matches!(got, Err(SmartObjectDefError::BadDefinition(_))),
            "{got:?}"
        );
        Ok(())
    }
}
