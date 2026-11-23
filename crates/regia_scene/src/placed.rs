// SPDX-License-Identifier: MPL-2.0
//! Placed levels: a saved level placed inside another level as one transformed object
//! (`docs/specs/37-per-entity-levels-layers-instances.md` § 6).
//!
//! An entity with [`PlacedLevel`] and a `Transform` is the *instance*. [`spawn_placed_levels`]
//! (registered in `PreUpdate` after the level lifecycle by [`crate::ScenePlugin`]) fills it:
//!
//! - [`PlacedMode::Inline`] spawns every entity of the referenced level that passes the
//!   [`PlacedFilter`] as a child subtree of the instance. Each spawned guid is
//!   [`derive_guid`]`(placement, source)`, so the same instance always spawns the same ids and two
//!   instances never collide. The referenced level's resources are ignored (one diagnostic per
//!   load); members carry [`PlacedMember`] and inherit the instance's `LayerMembership`.
//! - [`PlacedMode::Streamed`] opens the referenced level additively through the
//!   [`LevelManager`], parents its roots under the instance, and writes [`PlacedLevelLoaded`]. The
//!   level unloads when the instance despawns.
//!
//! The instance entity must carry a non-nil [`EntityGuid`]: inline member ids derive from it.
//! A missing guid, or [`Guid::NIL`] (`EntityGuid`'s default), is refused
//! (`missing_instance_guid`) instead of falling back to process-local entity ids or a shared nil
//! id. Process-local ids would depend on allocator state (two lockstep peers that spawned a
//! different number of entities would derive different ids); a nil id would make every such
//! instance derive the same member ids.
//!
//! Nesting depth is at most [`MAX_PLACED_DEPTH`] and one instance spawns at most
//! [`MAX_PLACED_ENTITIES`]; a cycle is refused (`placed_cycle`). A missing level leaves the
//! instance in place with [`PlacedError`]; the game keeps running. [`break_placed`] turns an inline
//! instance into plain entities.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use regia_core::id::Guid;
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::components::EntityGuid;
use crate::format::{SceneEntity, SceneFile};
use crate::layers::{LAYER_MEMBERSHIP_PATH, LayerMembership, LayerTable};
use crate::level::{LevelId, LevelLibrary, LevelManager, LevelMember, LevelState, LoadMode};
use crate::spawn::SpawnOptions;

/// Deepest nesting of placed levels inside placed levels.
pub const MAX_PLACED_DEPTH: usize = 8;
/// Most entities one instance may spawn (all nesting levels together).
pub const MAX_PLACED_ENTITIES: usize = 100_000;
/// Type path of [`PlacedLevel`] in scene records.
pub const PLACED_LEVEL_PATH: &str = "regia_scene::placed::PlacedLevel";

/// How a placed level is brought into the world.
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlacedMode {
    /// Spawned as a child subtree of the instance (default).
    #[default]
    Inline,
    /// Loaded through the level manager as an additive level parented under the instance.
    Streamed,
}

/// One layer rule of a [`PlacedFilter`].
#[derive(Reflect, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[reflect(Default)]
pub struct PlacedLayerRule {
    /// Layer name.
    pub layer: String,
    /// `true`: include members of the layer; `false`: exclude them.
    pub include: bool,
}

/// Which contained entities participate: with include rules, an entity must be in at least one
/// included layer; an entity in any excluded layer is dropped (with its descendants). Empty means
/// everything.
#[derive(Reflect, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[reflect(Default)]
#[serde(default)]
pub struct PlacedFilter {
    /// The rules.
    pub layers: Vec<PlacedLayerRule>,
}

impl PlacedFilter {
    /// Whether a record with `membership` passes.
    pub fn passes(&self, membership: Option<&LayerMembership>) -> bool {
        let names: &[String] = membership.map_or(&[], |m| m.layers.as_slice());
        if self.layers.iter().any(|r| !r.include && names.contains(&r.layer)) {
            return false;
        }
        let includes: Vec<&PlacedLayerRule> = self.layers.iter().filter(|r| r.include).collect();
        includes.is_empty() || includes.iter().any(|r| names.contains(&r.layer))
    }
}

/// A reference to another level placed in this one as a single transformed object.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[reflect(Component, Default)]
#[serde(default)]
pub struct PlacedLevel {
    /// The level asset (`asset:<guid>`).
    pub level: String,
    /// Inline (default) or streamed.
    pub mode: PlacedMode,
    /// Which contained entities participate.
    pub filter: PlacedFilter,
}

impl PlacedLevel {
    /// An inline placement of `level` with no filter.
    pub fn inline(level: LevelId) -> Self {
        Self {
            level: format!("asset:{}", level.0),
            mode: PlacedMode::Inline,
            filter: PlacedFilter::default(),
        }
    }

    /// The referenced level id (`None` when `level` is not a guid).
    pub fn level_id(&self) -> Option<LevelId> {
        let text = self.level.strip_prefix("asset:").unwrap_or(&self.level);
        text.parse().ok().map(LevelId)
    }

    /// Reads a placement from a scene record (`None` when absent or unparsable).
    pub fn from_record(record: &SceneEntity) -> Option<Self> {
        record
            .components
            .get(PLACED_LEVEL_PATH)
            .and_then(|raw| ron::from_str(raw.get_ron()).ok())
    }
}

/// Marks an entity spawned by a placed level; holds the instance entity. Placed members are shown
/// nested and read-only in the editor outliner.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlacedMember {
    /// The instance entity.
    pub instance: Entity,
}

/// A placement that could not be filled: the reason string (`level_not_found`, `spawn_failed`,
/// `placed_cycle`, `placed_limit`, `missing_instance_guid`) and detail. Remove it to retry.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub struct PlacedError {
    /// Agent `data.reason` string.
    pub reason: &'static str,
    /// Human-readable detail (the cycle chain, the limit hit).
    pub detail: String,
}

/// What a filled placement spawned.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlacedSpawned {
    /// The referenced level.
    pub level: LevelId,
    /// The mode it was filled with.
    pub mode: PlacedMode,
    /// Entities spawned (inline) or members of the streamed level.
    pub entities: usize,
}

/// A streamed placed level reached `Active` and was parented under its instance.
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlacedLevelLoaded {
    /// The instance entity.
    pub owner: Entity,
    /// The placed level.
    pub level: LevelId,
}

/// Streamed placements by instance, so a despawned instance unloads its level.
#[derive(Resource, Clone, Debug, Default)]
pub struct PlacedRegistry {
    streamed: BTreeMap<Entity, LevelId>,
}

/// Errors of placed-level operations; [`PlacedProblem::reason`] is the agent reason.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlacedProblem {
    /// The referenced level is not in the [`LevelLibrary`].
    #[error("level_not_found: {0}")]
    LevelNotFound(String),
    /// The referenced level is in the [`LevelLibrary`] but its scene failed to spawn or open
    /// (validation, prefab nesting, work budget); the detail carries the spawn error.
    #[error("spawn_failed: {0}")]
    Spawn(String),
    /// The level contains itself, directly or transitively.
    #[error("placed_cycle: {0}")]
    Cycle(String),
    /// Nesting depth or entity count exceeded.
    #[error("placed_limit: {0}")]
    Limit(String),
    /// The instance entity has no `EntityGuid`, or it is nil, so member ids cannot be derived
    /// deterministically; set a real one and remove the `PlacedError` to retry.
    #[error("missing_instance_guid: {0:?}")]
    MissingInstanceGuid(Entity),
    /// The entity is not a placed-level instance (or not an inline one, for `break`).
    #[error("not_placed: {0:?}")]
    NotPlaced(Entity),
}

impl PlacedProblem {
    /// The agent `data.reason` string.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::LevelNotFound(_) => "level_not_found",
            Self::Spawn(_) => "spawn_failed",
            Self::Cycle(_) => "placed_cycle",
            Self::Limit(_) => "placed_limit",
            Self::MissingInstanceGuid(_) => "missing_instance_guid",
            Self::NotPlaced(_) => "not_placed",
        }
    }
}

/// The guid of a contained entity: the first 16 bytes of `blake3(placement ‖ source)` (both
/// big-endian), so ids are stable per instance and distinct across instances.
pub fn derive_guid(placement: Guid, source: Guid) -> Guid {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&placement.as_u128().to_be_bytes());
    hasher.update(&source.as_u128().to_be_bytes());
    let hash = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    Guid::from_u128(u128::from_be_bytes(bytes))
}

/// Walks the placement graph from `level` (whose chain of hosts is `chain`): refuses a cycle and
/// a nesting depth over [`MAX_PLACED_DEPTH`]; returns the total entities the placement spawns
/// (all nesting levels), refusing more than [`MAX_PLACED_ENTITIES`]. Records count once each,
/// the added children of prefab-instance records included (placements authored there are walked
/// too); a prefab's own template entities are not counted here (each instantiation has its own
/// work bound): [`check_placement_with_prefabs`] counts them too, and is what a fill uses. Levels
/// not in the library count as zero here (they fail with `level_not_found` when filled).
pub fn check_placement(
    library: &LevelLibrary,
    level: LevelId,
    chain: &[LevelId],
) -> Result<usize, PlacedProblem> {
    walk_placement(library, None, level, chain)
}

/// [`check_placement`] that also counts what each prefab-instance record instantiates (v0.5.1):
/// the template's entities (a variant resolved to its base), nested prefabs to any depth, the
/// added children of variant steps and nested instances, without the removed children and their
/// subtrees. This is the count spec 37 § 6.4's cap is about, and what a fill checks before
/// anything spawns; a placement over [`MAX_PLACED_ENTITIES`] fails with `placed_limit`.
///
/// A prefab that `prefabs` does not hold (not loaded yet, or a broken variant chain) counts as
/// zero, like a missing level: instantiation reports it, and a prefab that arrives later spawns
/// through pending resolution with its own per-instantiation work bound. Nesting deeper than
/// `prefab::MAX_NESTING_DEPTH` is not counted further (instantiation refuses it).
pub fn check_placement_with_prefabs(
    library: &LevelLibrary,
    prefabs: &crate::prefab::PrefabLibrary,
    level: LevelId,
    chain: &[LevelId],
) -> Result<usize, PlacedProblem> {
    walk_placement(library, Some(prefabs), level, chain)
}

/// Counts one more spawned entity against [`MAX_PLACED_ENTITIES`].
fn count_entity(total: &mut usize) -> Result<(), PlacedProblem> {
    *total += 1;
    if *total > MAX_PLACED_ENTITIES {
        return Err(PlacedProblem::Limit(format!(
            "more than {MAX_PLACED_ENTITIES} entities"
        )));
    }
    Ok(())
}

/// Counts the entities instantiating `instance` spawns below its record (the record is counted
/// by the caller), mirroring `prefab::instantiate_prefab`: template entities in order, a removed
/// local id pruning its subtree (`removed` holds the enclosing instances' removed ids, which reach
/// into nested prefabs too), nested prefabs, then each variant step's added children. With
/// `own_added` the instance's added children are counted as well (a nested, template-owned
/// instance; a scene record's added children are scene records the caller walks).
fn count_prefab(
    prefabs: &crate::prefab::PrefabLibrary,
    instance: &crate::format::PrefabInstanceData,
    removed: &std::collections::HashSet<Guid>,
    own_added: bool,
    depth: u32,
    total: &mut usize,
) -> Result<(), PlacedProblem> {
    if depth > crate::prefab::MAX_NESTING_DEPTH {
        return Ok(());
    }
    let Ok(resolved) = prefabs.resolve(instance.prefab.guid()) else {
        return Ok(());
    };
    let mut removed = removed.clone();
    removed.extend(
        resolved
            .chain
            .iter()
            .flat_map(|variant| variant.edits.removed_children.iter())
            .chain(&instance.removed_children)
            .copied(),
    );
    let mut kept = vec![false; resolved.template.entities.len()];
    for (index, entity) in resolved.template.entities.iter().enumerate() {
        if removed.contains(&entity.local_id) {
            continue;
        }
        if let Some(parent) = entity.parent
            && !kept.get(parent as usize).copied().unwrap_or(false)
        {
            continue; // parent pruned
        }
        kept[index] = true;
        count_entity(total)?;
        if let Some(nested) = &entity.nested {
            count_prefab(prefabs, &nested.instance, &removed, true, depth + 1, total)?;
        }
    }
    for variant in &resolved.chain {
        count_records(prefabs, &variant.edits.added_children, depth + 1, total)?;
    }
    if own_added {
        count_records(prefabs, &instance.added_children, depth + 1, total)?;
    }
    Ok(())
}

/// Counts prefab-internal records (added children of a variant step or a nested instance) with
/// their own added children and prefab expansions.
fn count_records(
    prefabs: &crate::prefab::PrefabLibrary,
    records: &[SceneEntity],
    depth: u32,
    total: &mut usize,
) -> Result<(), PlacedProblem> {
    let mut stack: Vec<&SceneEntity> = records.iter().rev().collect();
    while let Some(record) = stack.pop() {
        count_entity(total)?;
        if let Some(instance) = &record.prefab_instance {
            count_prefab(prefabs, instance, &Default::default(), false, depth, total)?;
            stack.extend(instance.added_children.iter().rev());
        }
    }
    Ok(())
}

/// The walk behind [`check_placement`] and [`check_placement_with_prefabs`].
fn walk_placement(
    library: &LevelLibrary,
    prefabs: Option<&crate::prefab::PrefabLibrary>,
    level: LevelId,
    chain: &[LevelId],
) -> Result<usize, PlacedProblem> {
    fn walk(
        library: &LevelLibrary,
        prefabs: Option<&crate::prefab::PrefabLibrary>,
        level: LevelId,
        chain: &mut Vec<LevelId>,
        total: &mut usize,
    ) -> Result<(), PlacedProblem> {
        if chain.contains(&level) {
            let mut names: Vec<String> = chain.iter().map(|l| l.0.to_string()).collect();
            names.push(level.0.to_string());
            return Err(PlacedProblem::Cycle(names.join(" -> ")));
        }
        // `chain` holds the host plus every enclosing placement: depth = chain.len().
        if chain.len() > MAX_PLACED_DEPTH {
            return Err(PlacedProblem::Limit(format!(
                "nesting depth over {MAX_PLACED_DEPTH} at level {}",
                level.0
            )));
        }
        let Some(data) = library.get(level) else {
            return Ok(());
        };
        // Every record, including the added children of prefab-instance records (at any depth):
        // each is an entity, and each may place a level.
        // A stack popped in file pre-order, so nested levels are walked in file order.
        let mut records: Vec<&SceneEntity> = data.scene.entities.iter().rev().collect();
        let mut nested_levels = Vec::new();
        while let Some(record) = records.pop() {
            count_entity(total)?;
            if let Some(nested) = PlacedLevel::from_record(record).and_then(|p| p.level_id()) {
                nested_levels.push(nested);
            }
            if let Some(instance) = &record.prefab_instance {
                if let Some(prefabs) = prefabs {
                    count_prefab(prefabs, instance, &Default::default(), false, 0, total)?;
                }
                records.extend(instance.added_children.iter().rev());
            }
        }
        chain.push(level);
        for nested in nested_levels {
            walk(library, prefabs, nested, chain, total)?;
        }
        chain.pop();
        Ok(())
    }
    let mut chain = chain.to_vec();
    let mut total = 0;
    walk(library, prefabs, level, &mut chain, &mut total)?;
    Ok(total)
}

/// The hosts above `instance`: the levels of enclosing placements (innermost last), starting
/// with the host level the outermost instance belongs to.
fn host_chain(world: &World, instance: Entity) -> Vec<LevelId> {
    let mut chain = Vec::new();
    let mut cursor = instance;
    for _ in 0..=MAX_PLACED_DEPTH + 1 {
        match world.get::<PlacedMember>(cursor) {
            Some(member) => {
                if let Some(level) = world
                    .get::<PlacedLevel>(member.instance)
                    .and_then(PlacedLevel::level_id)
                {
                    chain.push(level);
                }
                cursor = member.instance;
            }
            None => {
                if let Some(level) = world.get::<LevelMember>(cursor) {
                    chain.push(level.level());
                }
                break;
            }
        }
    }
    chain.reverse();
    chain
}

/// Fills new placements and unloads the levels of despawned streamed instances. Instances are
/// processed in guid order (nested placements spawned by an inline fill are filled in the same
/// call), so the result is deterministic.
pub fn spawn_placed_levels(world: &mut World) {
    unload_orphaned_streams(world);
    // Nested placements appear as their hosts fill: loop until nothing new (bounded by depth).
    for _ in 0..=MAX_PLACED_DEPTH + 1 {
        let mut pending: Vec<(Guid, Entity)> = Vec::new();
        let mut unidentified: Vec<Entity> = Vec::new();
        {
            let mut query = QueryState::<
                (Entity, Option<&EntityGuid>),
                (With<PlacedLevel>, Without<PlacedSpawned>, Without<PlacedError>),
            >::new(world);
            for (entity, guid) in query.iter(world) {
                match guid {
                    // Member ids derive from the instance's guid. Process-local `Entity` bits
                    // are never a substitute, and nil (the `EntityGuid` default) is not an
                    // identity: every nil instance would derive the same member ids.
                    Some(guid) if !guid.0.is_nil() => pending.push((guid.0, entity)),
                    _ => unidentified.push(entity),
                }
            }
        }
        if pending.is_empty() && unidentified.is_empty() {
            return;
        }
        pending.sort();
        let _span = tracing::info_span!("placed_spawn", instances = pending.len()).entered();
        for instance in unidentified {
            let problem = PlacedProblem::MissingInstanceGuid(instance);
            tracing::warn!(target: "regia_scene", "placed level on {instance:?}: {problem}");
            if let Ok(mut entity) = world.get_entity_mut(instance) {
                entity.insert(PlacedError {
                    reason: problem.reason(),
                    detail: problem.to_string(),
                });
            }
        }
        for (placement, instance) in pending {
            let result = fill(world, instance, placement);
            if let Err(problem) = result
                && let Ok(mut entity) = world.get_entity_mut(instance)
            {
                tracing::warn!(target: "regia_scene", "placed level on {instance:?}: {problem}");
                entity.insert(PlacedError {
                    reason: problem.reason(),
                    detail: problem.to_string(),
                });
            }
        }
    }
}

fn fill(world: &mut World, instance: Entity, placement: Guid) -> Result<(), PlacedProblem> {
    let Some(placed) = world.get::<PlacedLevel>(instance).cloned() else {
        return Ok(());
    };
    let Some(level_id) = placed.level_id() else {
        return Err(PlacedProblem::LevelNotFound(placed.level.clone()));
    };
    let chain = host_chain(world, instance);
    let library = world.get_resource::<LevelLibrary>().cloned().unwrap_or_default();
    match world.get_resource::<crate::prefab::PrefabLibrary>() {
        Some(prefabs) => check_placement_with_prefabs(&library, prefabs, level_id, &chain)?,
        None => check_placement(&library, level_id, &chain)?,
    };
    let Some(level) = library.get(level_id).cloned() else {
        return Err(PlacedProblem::LevelNotFound(placed.level.clone()));
    };
    match placed.mode {
        PlacedMode::Inline => fill_inline(world, instance, placement, &placed, level_id, &level.scene),
        PlacedMode::Streamed => fill_streamed(world, instance, level_id),
    }
}

fn fill_inline(
    world: &mut World,
    instance: Entity,
    placement: Guid,
    placed: &PlacedLevel,
    level_id: LevelId,
    scene: &SceneFile,
) -> Result<(), PlacedProblem> {
    if !scene.resources.is_empty() {
        let names: Vec<&String> = scene.resources.keys().collect();
        tracing::warn!(
            target: "regia_scene",
            "placed level {}: its resources are ignored (the host level's settings win): {names:?}",
            level_id.0
        );
    }
    // Filter, dropping descendants of dropped records.
    let mut kept: BTreeSet<Guid> = scene
        .entities
        .iter()
        .filter(|r| placed.filter.passes(LayerMembership::from_record(r).as_ref()))
        .map(|r| r.guid)
        .collect();
    loop {
        let before = kept.len();
        for record in &scene.entities {
            if record.parent.is_some_and(|p| !kept.contains(&p)) {
                kept.remove(&record.guid);
            }
        }
        if kept.len() == before {
            break;
        }
    }
    let inherited = world.get::<LayerMembership>(instance).cloned();
    let host_level = world.get::<LevelMember>(instance).map(LevelMember::level);
    let map: HashMap<Guid, Guid> = kept.iter().map(|g| (*g, derive_guid(placement, *g))).collect();
    let mut file = SceneFile::new();
    for record in scene.entities.iter().filter(|r| kept.contains(&r.guid)) {
        let mut record = record.clone();
        record.guid = derive_guid(placement, record.guid);
        record.parent = Some(
            record
                .parent
                .and_then(|p| map.get(&p).copied())
                .unwrap_or(placement),
        );
        if let Some(instance_data) = &mut record.prefab_instance {
            for child in &mut instance_data.added_children {
                derive_added(child, placement, &map);
            }
        }
        if let Some(inherited) = &inherited {
            let mut membership = LayerMembership::from_record(&record).unwrap_or_default();
            for layer in &inherited.layers {
                if !membership.layers.contains(layer) {
                    membership.layers.push(layer.clone());
                }
            }
            if let Ok(text) = ron::to_string(&membership)
                && let Ok(raw) = ron::value::RawValue::from_boxed_ron(text.into_boxed_str())
            {
                record.components.insert(LAYER_MEMBERSHIP_PATH.to_owned(), raw);
            }
        }
        file.entities.push(record);
    }
    file.entities.push(SceneEntity {
        guid: placement,
        ..SceneEntity::default()
    });
    file.entities.sort_by_key(|e| e.guid);
    // What already hangs under the instance before the fill (an entity the host level authored
    // there, or one the user put there) is not a member: the tagging below skips it and its
    // subtree, so a save keeps it and a Break leaves it in place.
    let authored: BTreeSet<Entity> = {
        let mut out = BTreeSet::new();
        let mut stack: Vec<Entity> = world
            .get::<Children>(instance)
            .map(|c| c.iter().collect())
            .unwrap_or_default();
        while let Some(entity) = stack.pop() {
            if out.insert(entity)
                && let Some(children) = world.get::<Children>(entity)
            {
                stack.extend(children.iter());
            }
        }
        out
    };
    let adopt = HashMap::from([(placement, instance)]);
    let opts = SpawnOptions {
        level: host_level,
        ..Default::default()
    };
    let spawned = crate::spawn::spawn_scene_adopting(world, &file, &opts, &adopt).map_err(|e| match e {
        crate::spawn::SceneSpawnError::PrefabNestingTooDeep(_) => {
            PlacedProblem::Limit(format!("{}: {e}", placed.level))
        }
        // The level was found in the library; the failure is its spawn, not its absence.
        _ => PlacedProblem::Spawn(format!("{}: {e}", placed.level)),
    })?;
    for warning in &spawned.warnings {
        tracing::warn!(target: "regia_scene", "placed level {}: {warning}", level_id.0);
    }
    // Tag every spawned entity (prefab internals included) under the instance, not what was
    // there before the fill.
    let mut members = Vec::new();
    let mut stack: Vec<Entity> = world
        .get::<Children>(instance)
        .map(|c| c.iter().collect())
        .unwrap_or_default();
    while let Some(entity) = stack.pop() {
        if authored.contains(&entity) || world.get::<PlacedMember>(entity).is_some() {
            continue;
        }
        members.push(entity);
        if let Some(children) = world.get::<Children>(entity) {
            stack.extend(children.iter());
        }
    }
    for entity in &members {
        if let Ok(mut e) = world.get_entity_mut(*entity) {
            e.insert(PlacedMember { instance });
        }
    }
    if let Ok(mut e) = world.get_entity_mut(instance) {
        e.insert(PlacedSpawned {
            level: level_id,
            mode: PlacedMode::Inline,
            entities: spawned.entities,
        });
    }
    if let Some(mut table) = world.get_resource_mut::<LayerTable>() {
        table.mark_dirty();
    }
    Ok(())
}

fn derive_added(child: &mut SceneEntity, placement: Guid, map: &HashMap<Guid, Guid>) {
    child.guid = derive_guid(placement, child.guid);
    if let Some(parent) = child.parent
        && let Some(mapped) = map.get(&parent)
    {
        child.parent = Some(*mapped);
    }
    if let Some(instance) = &mut child.prefab_instance {
        for nested in &mut instance.added_children {
            derive_added(nested, placement, map);
        }
    }
}

fn fill_streamed(world: &mut World, instance: Entity, level_id: LevelId) -> Result<(), PlacedProblem> {
    let already = world
        .get_resource::<LevelManager>()
        .and_then(|m| m.state(level_id))
        .is_some();
    if already {
        return Err(PlacedProblem::Limit(format!(
            "level {} is already loaded; a level streams once at a time",
            level_id.0
        )));
    }
    // Registered before the open: the level's file is authored relative to this instance, so the
    // open leaves its roots unshifted by the floating origin (see `streamed_by`).
    world
        .get_resource_or_init::<PlacedRegistry>()
        .streamed
        .insert(instance, level_id);
    let opened = crate::level::open_level(world, level_id, LoadMode::Additive)
        // The level was found in the library (fill checked); the failure is its open.
        .map_err(|e| PlacedProblem::Spawn(format!("{}: {e}", level_id.0)))
        .and_then(|()| {
            if world
                .get_resource::<LevelManager>()
                .and_then(|m| m.state(level_id))
                == Some(LevelState::Active)
            {
                Ok(())
            } else {
                Err(PlacedProblem::LevelNotFound(level_id.0.to_string()))
            }
        });
    if let Err(problem) = opened {
        world
            .get_resource_or_init::<PlacedRegistry>()
            .streamed
            .remove(&instance);
        return Err(problem);
    }
    let members = attach_streamed_roots(world, instance, level_id);
    if let Ok(mut e) = world.get_entity_mut(instance) {
        e.insert(PlacedSpawned {
            level: level_id,
            mode: PlacedMode::Streamed,
            entities: members,
        });
    }
    world.write_message(PlacedLevelLoaded {
        owner: instance,
        level: level_id,
    });
    Ok(())
}

/// The live instance that streams `level`, if any. Such a level's file is authored relative to
/// the instance, so its spawns (the open, a reload in place, a data-layer respawn) skip the
/// floating-origin shift and their roots go under the instance instead.
pub(crate) fn streamed_by(world: &World, level: LevelId) -> Option<Entity> {
    world.get_resource::<PlacedRegistry>().and_then(|registry| {
        registry.streamed.iter().find_map(|(instance, streamed)| {
            (*streamed == level && world.get_entity(*instance).is_ok()).then_some(*instance)
        })
    })
}

/// Parents the roots of a streamed level under its instance, makes every entity of the level a
/// [`PlacedMember`] of the instance (like the inline fill, so a full-world snapshot writes none of
/// them), and answers the member count. Members of placements nested in the level keep their own
/// instance.
fn attach_streamed_roots(world: &mut World, instance: Entity, level_id: LevelId) -> usize {
    let mut roots: Vec<(Guid, Entity)> = {
        let mut query =
            QueryState::<(Entity, &LevelMember, Option<&EntityGuid>), Without<ChildOf>>::new(world);
        query
            .iter(world)
            .filter(|(_, m, _)| m.level() == level_id)
            .map(|(e, _, g)| (g.map_or(Guid::NIL, |g| g.0), e))
            .collect()
    };
    roots.sort();
    let mut members = 0;
    {
        let mut query = QueryState::<&LevelMember>::new(world);
        for member in query.iter(world) {
            if member.level() == level_id {
                members += 1;
            }
        }
    }
    for (_, root) in &roots {
        if let Ok(mut e) = world.get_entity_mut(*root) {
            e.insert(ChildOf(instance));
        }
    }
    // Every level root under the instance (earlier ones too: a data-layer respawn adds children
    // under them), walked down to the leaves.
    let mut stack: Vec<Entity> = world
        .get::<Children>(instance)
        .map(|children| {
            children
                .iter()
                .filter(|child| {
                    world
                        .get::<LevelMember>(*child)
                        .is_some_and(|m| m.level() == level_id)
                })
                .collect()
        })
        .unwrap_or_default();
    let mut tag = Vec::new();
    while let Some(entity) = stack.pop() {
        match world.get::<PlacedMember>(entity) {
            // A nested placement's members (and their subtrees) belong to it.
            Some(member) if member.instance != instance => continue,
            Some(_) => {}
            None => tag.push(entity),
        }
        if let Some(children) = world.get::<Children>(entity) {
            stack.extend(children.iter());
        }
    }
    for entity in tag {
        if let Ok(mut e) = world.get_entity_mut(entity) {
            e.insert(PlacedMember { instance });
        }
    }
    members
}

/// Puts a streamed level's roots back under the instance that streams it. A reopen under the same
/// id (a reload on change) respawns them as roots; the instance stays filled, so nothing else
/// would. Does nothing for a level no instance streams.
pub fn reattach_streamed(world: &mut World, level_id: LevelId) {
    let Some(instance) = streamed_by(world, level_id) else {
        return;
    };
    let members = attach_streamed_roots(world, instance, level_id);
    if let Some(mut spawned) = world.get_mut::<PlacedSpawned>(instance) {
        spawned.entities = members;
    }
}

fn unload_orphaned_streams(world: &mut World) {
    let Some(registry) = world.get_resource::<PlacedRegistry>() else {
        return;
    };
    let gone: Vec<(Entity, LevelId)> = registry
        .streamed
        .iter()
        .filter(|(e, _)| world.get_entity(**e).is_err())
        .map(|(e, l)| (*e, *l))
        .collect();
    for (entity, level) in gone {
        if let Err(err) = crate::level::unload_level(world, level) {
            tracing::warn!(target: "regia_scene", "unloading streamed placed level {}: {err}", level.0);
        }
        world
            .get_resource_or_init::<PlacedRegistry>()
            .streamed
            .remove(&entity);
    }
}

/// Replaces an inline instance with plain entities: members lose [`PlacedMember`], the derived
/// guids become permanent, the instance's direct children move to the instance's parent keeping
/// their world transforms, and the instance entity is despawned. The source level is untouched.
/// Returns the guids of the former members, sorted.
pub fn break_placed(world: &mut World, instance: Entity) -> Result<Vec<Guid>, PlacedProblem> {
    let inline = world
        .get::<PlacedSpawned>(instance)
        .is_some_and(|s| s.mode == PlacedMode::Inline);
    if !inline {
        return Err(PlacedProblem::NotPlaced(instance));
    }
    let mut members: Vec<Entity> = {
        let mut query = QueryState::<(Entity, &PlacedMember)>::new(world);
        query
            .iter(world)
            .filter(|(_, m)| m.instance == instance)
            .map(|(e, _)| e)
            .collect()
    };
    members.sort();
    let new_parent = world.get::<ChildOf>(instance).map(ChildOf::parent);
    let mut guids = Vec::new();
    for entity in members {
        let direct = world.get::<ChildOf>(entity).map(ChildOf::parent) == Some(instance);
        let local = direct.then(|| crate::hierarchy::local_under_parent(world, entity, new_parent));
        let guid = world.get::<EntityGuid>(entity).map(|g| g.0);
        if let Ok(mut e) = world.get_entity_mut(entity) {
            e.remove::<PlacedMember>();
            if let Some(local) = local {
                match new_parent {
                    Some(parent) => {
                        e.insert((local, ChildOf(parent)));
                    }
                    None => {
                        e.insert(local);
                        e.remove::<ChildOf>();
                    }
                }
            }
        }
        guids.extend(guid);
    }
    // What else hangs under the instance (an entity the host level or the user put there) is
    // not a member, but Break replaces the instance with plain entities: it moves to the
    // instance's parent at the same world place instead of going with the instance's despawn.
    let others: Vec<Entity> = world
        .get::<Children>(instance)
        .map(|children| children.to_vec())
        .unwrap_or_default();
    for entity in others {
        let local = crate::hierarchy::local_under_parent(world, entity, new_parent);
        if let Ok(mut e) = world.get_entity_mut(entity) {
            match new_parent {
                Some(parent) => {
                    e.insert((local, ChildOf(parent)));
                }
                None => {
                    e.insert(local);
                    e.remove::<ChildOf>();
                }
            }
        }
    }
    if let Ok(e) = world.get_entity_mut(instance) {
        e.despawn();
    }
    guids.sort();
    Ok(guids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_guids_are_stable_and_distinct() {
        let a = Guid::from_u128(1);
        let b = Guid::from_u128(2);
        let source = Guid::from_u128(0xabc);
        assert_eq!(derive_guid(a, source), derive_guid(a, source));
        assert_ne!(derive_guid(a, source), derive_guid(b, source));
        assert_ne!(derive_guid(a, source), derive_guid(a, Guid::from_u128(0xabd)));
    }

    /// `EntityGuid` defaults to nil. A nil placement id is not an identity: two such instances
    /// would derive the same member guids. Refuse it the same way as a missing guid, before fill
    /// looks the level up.
    #[test]
    fn nil_placed_guid_is_refused_before_fill() {
        let mut world = World::new();
        let instance = world
            .spawn((
                EntityGuid::default(),
                PlacedLevel::inline(LevelId(Guid::from_u128(9))),
            ))
            .id();
        spawn_placed_levels(&mut world);
        let err = world.get::<PlacedError>(instance).expect("placed error");
        assert_eq!(err.reason, "missing_instance_guid");
        assert!(world.get::<PlacedSpawned>(instance).is_none());
    }

    #[test]
    fn filters_include_and_exclude() {
        let m = |l: &[&str]| LayerMembership {
            layers: l.iter().map(|s| (*s).to_owned()).collect(),
        };
        let all = PlacedFilter::default();
        assert!(all.passes(None));
        let only_night = PlacedFilter {
            layers: vec![PlacedLayerRule {
                layer: "night".into(),
                include: true,
            }],
        };
        assert!(only_night.passes(Some(&m(&["night"]))));
        assert!(!only_night.passes(Some(&m(&["day"]))));
        assert!(!only_night.passes(None));
        let no_debug = PlacedFilter {
            layers: vec![PlacedLayerRule {
                layer: "debug".into(),
                include: false,
            }],
        };
        assert!(no_debug.passes(None));
        assert!(!no_debug.passes(Some(&m(&["debug", "night"]))));
    }
}
