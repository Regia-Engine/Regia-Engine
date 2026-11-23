// SPDX-License-Identifier: MPL-2.0
//! Incremental level autosave (ADR-7748, `docs/specs/19-editor.md` § 7).
//!
//! The autosave tick snapshots the open level on the main thread (the level lives in the world)
//! and hands the RON encoding and the write to the autosave worker ([`crate::doc_autosave`]).
//! Reflecting and serializing every component of every entity was most of a big level's hitch
//! (about 645 ms median on a 20 000-entity level, ADR-9770). [`LevelSnapshotCache`] keeps each
//! entity's serialized components between ticks, and the next tick reflects only the entities
//! whose saved components changed since the last snapshot, by the world's change ticks: an
//! unchanged entity's components come from the cache.
//!
//! The cache answers through [`regia_scene::spawn::SnapshotCache`]: everything else in a record
//! (guid, name, parent, prefab instance), the included set, the order and the level's resources
//! are read fresh on every tick, so the snapshot is the one a full snapshot gives, byte for byte.
//!
//! An entry holds while the entity has the same archetype (no component added or removed) and
//! none of the components it serialized was changed after the snapshot that wrote the entry. The
//! whole cache is dropped when the open level changes, when the type registry gains or loses
//! types (a newly registered type would start being written), and when the last snapshot is old
//! enough that the world's change ticks may have been clamped (`MAX_CHANGE_AGE`). A component
//! that changes through interior mutability without a change tick is not seen: the engine's saved
//! components carry none.

use std::collections::HashMap;

use regia_ecs::ecs::archetype::ArchetypeId;
use regia_ecs::ecs::change_detection::{MAX_CHANGE_AGE, Tick};
use regia_ecs::ecs::component::ComponentId;
use regia_ecs::prelude::*;
use regia_ecs::reflect::AppTypeRegistry;
use regia_scene::level::{Level, LevelId};
use regia_scene::spawn::{SceneSpawnError, SnapshotCache, SnapshotComponents};

/// What one snapshot through the cache did (counted, so tests bound the main thread's work by
/// entities rather than by time).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SnapshotStats {
    /// Entities whose components were reflected and serialized on the calling thread.
    pub reflected: u64,
    /// Entities whose components came from the cache.
    pub reused: u64,
    /// Whether the cache was dropped first (a full snapshot): first tick, another level, the
    /// type registry changed, or the last snapshot was too old for the change ticks.
    pub full: bool,
}

/// One entity's kept components.
struct Entry {
    archetype: ArchetypeId,
    components: SnapshotComponents,
    /// The components the entry was serialized from (their change ticks decide whether it holds).
    watched: Vec<ComponentId>,
    /// The snapshot that last used the entry (entries no snapshot used are pruned).
    seen: u64,
}

/// The serialized components of the open level's entities as of the last autosave snapshot
/// (a resource; see the module docs).
#[derive(Resource, Default)]
pub struct LevelSnapshotCache {
    level: Option<LevelId>,
    entries: HashMap<Entity, Entry>,
    /// The world's change tick when the last snapshot started: a change after it is newer.
    taken_at: Option<Tick>,
    registry_len: usize,
    epoch: u64,
    last: SnapshotStats,
    snapshots: u64,
}

impl std::fmt::Debug for LevelSnapshotCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LevelSnapshotCache")
            .field("level", &self.level)
            .field("entries", &self.entries.len())
            .field("last", &self.last)
            .field("snapshots", &self.snapshots)
            .finish_non_exhaustive()
    }
}

impl LevelSnapshotCache {
    /// What the last snapshot through the cache did.
    #[must_use]
    pub fn last_stats(&self) -> SnapshotStats {
        self.last
    }

    /// Snapshots taken through the cache this session.
    #[must_use]
    pub fn snapshots(&self) -> u64 {
        self.snapshots
    }

    /// Entities with kept components.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is kept.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drops every kept entry (the next snapshot is a full one).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.taken_at = None;
        self.level = None;
    }
}

/// One snapshot's view of the cache: the ticks it compares against and its counters.
struct Pass<'c> {
    cache: &'c mut LevelSnapshotCache,
    /// The previous snapshot's start (`None`: a full snapshot, nothing is reused).
    last_run: Option<Tick>,
    this_run: Tick,
    stats: SnapshotStats,
}

impl SnapshotCache for Pass<'_> {
    fn reuse(&mut self, world: &World, entity: Entity) -> Option<SnapshotComponents> {
        let last_run = self.last_run?;
        let entry = self.cache.entries.get_mut(&entity)?;
        let found = world.get_entity(entity).ok()?;
        if found.archetype().id() != entry.archetype {
            return None;
        }
        let changed = entry.watched.iter().any(|id| {
            found
                .get_change_ticks_by_id(*id)
                .is_none_or(|ticks| ticks.is_changed(last_run, self.this_run))
        });
        if changed {
            return None;
        }
        entry.seen = self.cache.epoch;
        self.stats.reused += 1;
        Some(entry.components.clone())
    }

    fn keep(
        &mut self,
        world: &World,
        entity: Entity,
        components: &SnapshotComponents,
        watched: &[ComponentId],
    ) {
        self.stats.reflected += 1;
        let Ok(found) = world.get_entity(entity) else {
            return;
        };
        self.cache.entries.insert(
            entity,
            Entry {
                archetype: found.archetype().id(),
                components: components.clone(),
                watched: watched.to_vec(),
                seen: self.cache.epoch,
            },
        );
    }
}

/// The number of types the world's registry holds (a registration added or dropped changes what
/// a snapshot writes, so the cache starts over).
fn registry_len(world: &World) -> usize {
    world
        .get_resource::<AppTypeRegistry>()
        .map_or(0, |registry| registry.read().iter().count())
}

/// Snapshots `level` through `cache` ([`regia_scene::level::snapshot_level_cached`]): the same
/// [`Level`] a full snapshot gives, reflecting only the entities that changed since the cache's
/// last snapshot. The caller strips editor-only entities and lifts sequence previews as a full
/// snapshot does ([`snapshot_saved_level`]).
///
/// # Errors
///
/// As [`regia_scene::level::snapshot_level`].
pub fn snapshot_level_through(
    world: &mut World,
    level: LevelId,
    cache: &mut LevelSnapshotCache,
) -> Result<Level, SceneSpawnError> {
    let _span = tracing::info_span!(target: "regia_editor", "autosave_snapshot").entered();
    let this_run = world.change_tick();
    // Every write from here on (the snapshot's own bookkeeping, a preview put back after it,
    // the next frame's edits) is newer than `this_run`, so the next snapshot sees it.
    world.increment_change_tick();
    let types = registry_len(world);
    let too_old = cache
        .taken_at
        .is_some_and(|taken| this_run.get().wrapping_sub(taken.get()) > MAX_CHANGE_AGE / 2);
    let full =
        cache.level != Some(level) || cache.registry_len != types || cache.taken_at.is_none() || too_old;
    if full {
        cache.entries.clear();
    }
    cache.epoch += 1;
    let epoch = cache.epoch;
    let mut pass = Pass {
        last_run: if full { None } else { cache.taken_at },
        this_run,
        stats: SnapshotStats {
            full,
            ..SnapshotStats::default()
        },
        cache: &mut *cache,
    };
    let result = regia_scene::level::snapshot_level_cached(world, level, Some(&mut pass));
    let stats = pass.stats;
    match &result {
        Ok(_) => {
            cache.entries.retain(|_, entry| entry.seen == epoch);
            cache.level = Some(level);
            cache.taken_at = Some(this_run);
            cache.registry_len = types;
        }
        // A failed snapshot leaves nothing to trust: start over next time.
        Err(_) => cache.clear(),
    }
    cache.last = stats;
    cache.snapshots += 1;
    tracing::debug!(
        target: "regia_editor",
        reflected = stats.reflected,
        reused = stats.reused,
        full = stats.full,
        "autosave snapshot"
    );
    result
}

/// The level as a save writes it ([`crate::level_io::snapshot_saved_level`]: no `EditorOnly`
/// entities, a running sequence preview's values lifted), snapshotted through the world's
/// [`LevelSnapshotCache`] (inserted on first use).
///
/// # Errors
///
/// As [`regia_scene::level::snapshot_level`].
pub fn snapshot_saved_level(world: &mut World, level: LevelId) -> Result<Level, SceneSpawnError> {
    let mut cache = world.remove_resource::<LevelSnapshotCache>().unwrap_or_default();
    let result = crate::seq_tracks::with_preview_lifted(world, |world| {
        snapshot_level_through(world, level, &mut cache).map(|mut snapshot| {
            crate::editor_only::strip_scene(world, &mut snapshot.scene);
            snapshot
        })
    });
    world.insert_resource(cache);
    result
}

/// What the last autosave snapshot did (`None` before the first one): for `editor.autosave_state`.
#[must_use]
pub fn last_stats(world: &World) -> Option<SnapshotStats> {
    world
        .get_resource::<LevelSnapshotCache>()
        .filter(|cache| cache.snapshots > 0)
        .map(LevelSnapshotCache::last_stats)
}

#[cfg(test)]
mod tests;
