// SPDX-License-Identifier: MPL-2.0
//! Spawnable bindings (`.seq.ron` v5, ADR-2948, book `agents/sequence-player.md`): a sequence
//! binding that names a prefab instead of an existing entity.
//!
//! - **Pre-roll.** [`SequencePlayer::play`] on a sequence with spawnables starts in
//!   [`PlaybackStatus::Waiting`] with the clock held ([`SequencePlayer::is_prerolling`]) until every
//!   prefab the sequence names is in the scene's [`PrefabLibrary`]: a project `.prefab.ron` is read
//!   and baked on a worker thread in real time, and on the calling thread when the time is manual
//!   (render-out, `play.step`, headless tests), the clock is external, or
//!   [`SequenceSpawnables::force_sync`] is set, so those runs start on the same frame every time. A
//!   `guid:` reference must already be in the library.
//! - **Spawn and despawn.** Each frame after the clocks advance, a spawnable whose range holds the
//!   clock ([`Spawnable::live_at`]) is instantiated (a wrapper entity with a deterministic
//!   [`EntityGuid`], a [`Transform`], the binding as its [`Name`] and [`SpawnedBySequence`], the
//!   prefab's entities under it), and one whose range does not hold the clock is despawned. Spawns
//!   go in the sequence's spawnable order, despawns in binding-name order, players in entity order,
//!   so a render-out and a repeated run create the same entities with the same guids in the same
//!   order. The binding then resolves to the wrapper for this frame's apply.
//! - **Stop.** A player that stops, lets go at the end ([`EndPolicy::Once`]) or is despawned
//!   despawns its actors, except those with `keep_on_stop`, which are left to the game.
//! - **Failures.** A prefab that does not load, or does not instantiate, is reported once on the
//!   player as [`SequenceIssue::SpawnFailed`] (kind `seq_spawn_failed`; codes in [`codes`]) and
//!   its binding stays unbound, so its tracks are skipped; the rest of the sequence plays.
//!
//! [`EndPolicy::Once`]: super::EndPolicy::Once

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_ecs::reflect::AppTypeRegistry;
use regia_scene::Transform;
use regia_scene::components::{EntityGuid, Name};
use regia_scene::format::{PrefabInstanceData, SceneAssetRef, SceneFile};
use regia_scene::prefab::{PrefabLibrary, PrefabTemplate, instantiate_prefab};
use regia_scene::spawn::SpawnOptions;

use super::asset::Spawnable;
use super::player::{PlaybackStatus, SequenceIssue, SequencePlayer};

/// The codes a [`SequenceIssue::SpawnFailed`] carries (agent kind `seq_spawn_failed`).
pub mod codes {
    /// The `.prefab.ron` file does not exist under the project root.
    pub const PREFAB_MISSING: &str = "prefab_missing";
    /// The file exists but could not be read.
    pub const PREFAB_READ: &str = "prefab_read";
    /// The file is not a valid scene/prefab document.
    pub const PREFAB_PARSE: &str = "prefab_parse";
    /// The document parsed but did not bake into a template (no single root, a bad component).
    pub const PREFAB_BAKE: &str = "prefab_bake";
    /// A `guid:` reference names a template that is not in the scene's prefab library.
    pub const PREFAB_NOT_REGISTERED: &str = "prefab_not_registered";
    /// The template is loaded but instantiating it failed (nesting, work budget, a bad edit).
    pub const PREFAB_SPAWN: &str = "prefab_spawn";
}

/// The result of loading one prefab: the baked template, or a code and a message.
type LoadResult = Result<PrefabTemplate, (&'static str, String)>;

/// Where one prefab reference is.
enum PrefabLoad {
    /// Reading and baking on a worker thread.
    #[cfg(not(target_arch = "wasm32"))]
    Loading(std::thread::JoinHandle<LoadResult>),
    /// In the [`PrefabLibrary`] under this asset guid.
    Ready(Guid),
    /// Failed with this code (and message).
    Failed(&'static str, String),
}

/// One live actor a player spawned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveActor {
    /// The wrapper entity the binding resolves to (the prefab's entities are its children).
    pub entity: Entity,
    /// Its deterministic guid.
    pub guid: Guid,
    /// The spawnable's `keep_on_stop`.
    pub keep_on_stop: bool,
}

/// On the wrapper entity of a spawned actor: which player spawned it under which binding.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct SpawnedBySequence {
    /// The player entity.
    pub player: Entity,
    /// The spawnable's binding name.
    pub binding: String,
}

/// A spawnable actor appeared or went away.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct SpawnableChanged {
    /// The player entity.
    pub player: Entity,
    /// The binding name.
    pub binding: String,
    /// The wrapper entity (already despawned when `spawned` is `false`, unless it was kept).
    pub entity: Entity,
    /// `true` on spawn, `false` on despawn or release.
    pub spawned: bool,
    /// `true` when the actor was left in the world (`keep_on_stop`) instead of despawned.
    pub kept: bool,
}

/// Counters for the agent and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpawnStats {
    /// Actors spawned since the app started.
    pub spawned: u64,
    /// Actors despawned.
    pub despawned: u64,
    /// Actors left in the world by `keep_on_stop`.
    pub kept: u64,
    /// Prefab files read and baked.
    pub loads: u64,
    /// Prefab loads that failed.
    pub load_failures: u64,
}

/// The spawnable runtime: prefab loads, live actors per player, settings.
#[derive(Resource, Default)]
pub struct SequenceSpawnables {
    /// Where `.prefab.ron` paths are read from; `None` uses the asset server's project root, then
    /// `regia_asset::detect_project_root`.
    pub project_root: Option<PathBuf>,
    /// `true` loads prefabs on the calling thread always (what manual time and external clocks do
    /// anyway), so the pre-roll ends on the frame it starts.
    pub force_sync: bool,
    loads: BTreeMap<String, PrefabLoad>,
    live: BTreeMap<Entity, BTreeMap<String, LiveActor>>,
    failed: BTreeMap<Entity, BTreeSet<String>>,
    stats: SpawnStats,
}

impl std::fmt::Debug for SequenceSpawnables {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SequenceSpawnables")
            .field("project_root", &self.project_root)
            .field("force_sync", &self.force_sync)
            .field("loads", &self.loads.len())
            .field("live", &self.live)
            .field("stats", &self.stats)
            .finish()
    }
}

impl SequenceSpawnables {
    /// The actors `player` has live now, by binding name.
    #[must_use]
    pub fn live(&self, player: Entity) -> Option<&BTreeMap<String, LiveActor>> {
        self.live.get(&player)
    }

    /// The live actor of one binding of one player.
    #[must_use]
    pub fn actor(&self, player: Entity, binding: &str) -> Option<LiveActor> {
        self.live.get(&player).and_then(|m| m.get(binding)).copied()
    }

    /// Where a prefab reference is: `"loading"`, `"ready"`, `"failed"` (with its code), or `None`
    /// when nothing asked for it yet.
    #[must_use]
    pub fn load_state(&self, prefab: &str) -> Option<(&'static str, Option<&'static str>)> {
        self.loads.get(prefab).map(|load| match load {
            #[cfg(not(target_arch = "wasm32"))]
            PrefabLoad::Loading(_) => ("loading", None),
            PrefabLoad::Ready(_) => ("ready", None),
            PrefabLoad::Failed(code, _) => ("failed", Some(*code)),
        })
    }

    /// `true` while a prefab reference loads on a worker thread.
    #[must_use]
    pub fn is_loading(&self, prefab: &str) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            matches!(self.loads.get(prefab), Some(PrefabLoad::Loading(_)))
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = prefab;
            false
        }
    }

    /// Counters.
    #[must_use]
    pub fn stats(&self) -> SpawnStats {
        self.stats
    }

    /// Forgets a prefab's load (a failure or a cached template guid), so the next pre-roll loads it
    /// again (after the file changed).
    pub fn forget(&mut self, prefab: &str) {
        self.loads.remove(prefab);
    }
}

/// FNV-1a 128 over byte strings: the deterministic actor guids.
fn fnv128(parts: &[&[u8]]) -> u128 {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    let mut hash = OFFSET;
    for part in parts {
        for &byte in *part {
            hash ^= u128::from(byte);
            hash = hash.wrapping_mul(PRIME);
        }
        // A separator so ("ab", "c") and ("a", "bc") differ.
        hash ^= 0xff;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// The guid of a spawned actor: a hash of the player's identity (its own guid, or the sequence
/// name), the binding and a generation that only moves past a guid already in the world (an actor
/// kept from an earlier run). Never nil.
#[must_use]
pub fn actor_guid(player_key: u128, binding: &str, generation: u32) -> Guid {
    let hash = fnv128(&[
        b"regia.seq.spawnable",
        &player_key.to_le_bytes(),
        binding.as_bytes(),
        &generation.to_le_bytes(),
    ]);
    Guid::from_u128(hash.max(1))
}

/// The guid a `.prefab.ron` without a `.meta` sidecar is registered under: a hash of its path.
#[must_use]
pub fn path_prefab_guid(path: &str) -> Guid {
    Guid::from_u128(fnv128(&[b"regia.seq.prefab", path.as_bytes()]).max(1))
}

/// Reads and bakes one project `.prefab.ron` (any thread).
fn load_prefab(root: &Path, path: &str, registry: &AppTypeRegistry) -> LoadResult {
    let _span = tracing::info_span!(target: "regia_framework", "sequencer_spawnable_load", %path).entered();
    let full = root.join(path);
    if !full.is_file() {
        return Err((
            codes::PREFAB_MISSING,
            format!("{} does not exist", full.display()),
        ));
    }
    let text = std::fs::read_to_string(&full)
        .map_err(|e| (codes::PREFAB_READ, format!("cannot read {}: {e}", full.display())))?;
    let file = SceneFile::from_ron_str(&text).map_err(|e| (codes::PREFAB_PARSE, e.to_string()))?;
    let meta_path = regia_asset::meta_path_for(&full);
    let guid = std::fs::read(&meta_path)
        .ok()
        .and_then(|bytes| regia_asset::MetaSidecar::from_ron(&bytes).ok())
        .map_or_else(|| path_prefab_guid(path), |meta| meta.id.0);
    let registry = registry.read();
    PrefabTemplate::from_scene_file(guid, &file, &registry).map_err(|e| (codes::PREFAB_BAKE, e.to_string()))
}

/// The project root prefab paths are read from.
fn project_root(world: &World, spawnables: &SequenceSpawnables) -> PathBuf {
    spawnables
        .project_root
        .clone()
        .or_else(|| {
            world
                .get_resource::<regia_asset::AssetServer>()
                .map(|s| s.project_root().to_path_buf())
        })
        .unwrap_or_else(regia_asset::detect_project_root)
}

/// Finishes or starts the load of one reference; returns its state after this call.
fn poll_load(world: &mut World, spawnables: &mut SequenceSpawnables, prefab: &str, sync: bool) {
    // A `guid:` reference names a template that something else registered.
    if let Some(guid) = Spawnable::new("", prefab, 0.0).prefab_guid() {
        let present = world
            .get_resource::<PrefabLibrary>()
            .is_some_and(|library| library.contains(guid));
        let state = if present {
            PrefabLoad::Ready(guid)
        } else {
            PrefabLoad::Failed(
                codes::PREFAB_NOT_REGISTERED,
                format!("no template {guid} in the PrefabLibrary"),
            )
        };
        spawnables.loads.insert(prefab.to_string(), state);
        return;
    }
    // A cached template the library lost (a level reset) loads again.
    if let Some(PrefabLoad::Ready(guid)) = spawnables.loads.get(prefab)
        && !world
            .get_resource::<PrefabLibrary>()
            .is_some_and(|library| library.contains(*guid))
    {
        spawnables.loads.remove(prefab);
    }
    let finished: Option<LoadResult> = match spawnables.loads.remove(prefab) {
        Some(PrefabLoad::Ready(guid)) => {
            spawnables
                .loads
                .insert(prefab.to_string(), PrefabLoad::Ready(guid));
            return;
        }
        Some(PrefabLoad::Failed(code, message)) => {
            spawnables
                .loads
                .insert(prefab.to_string(), PrefabLoad::Failed(code, message));
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        Some(PrefabLoad::Loading(handle)) => {
            if sync || handle.is_finished() {
                Some(handle.join().unwrap_or_else(|_| {
                    Err((codes::PREFAB_BAKE, "the prefab load thread panicked".to_string()))
                }))
            } else {
                spawnables
                    .loads
                    .insert(prefab.to_string(), PrefabLoad::Loading(handle));
                return;
            }
        }
        None => {
            let root = project_root(world, spawnables);
            let registry = world
                .get_resource::<AppTypeRegistry>()
                .cloned()
                .unwrap_or_default();
            spawnables.stats.loads += 1;
            #[cfg(not(target_arch = "wasm32"))]
            if !sync {
                let (path, thread_root, thread_registry) =
                    (prefab.to_string(), root.clone(), registry.clone());
                let spawned = std::thread::Builder::new()
                    .name("regia-seq-prefab".into())
                    .spawn(move || load_prefab(&thread_root, &path, &thread_registry));
                match spawned {
                    Ok(handle) => {
                        spawnables
                            .loads
                            .insert(prefab.to_string(), PrefabLoad::Loading(handle));
                        return;
                    }
                    // No thread: load here instead.
                    Err(error) => {
                        tracing::debug!(target: "regia_framework", "spawnable load thread: {error}");
                    }
                }
            }
            #[cfg(target_arch = "wasm32")]
            let _ = sync;
            Some(load_prefab(&root, prefab, &registry))
        }
    };
    let state = match finished {
        Some(Ok(template)) => {
            let guid = template.asset;
            world
                .get_resource_or_insert_with::<PrefabLibrary>(PrefabLibrary::default)
                .insert(template);
            PrefabLoad::Ready(guid)
        }
        Some(Err((code, message))) => {
            spawnables.stats.load_failures += 1;
            PrefabLoad::Failed(code, message)
        }
        None => return,
    };
    spawnables.loads.insert(prefab.to_string(), state);
}

/// `true` when this frame's loads must finish on the calling thread.
fn sync_loads(world: &World, spawnables: &SequenceSpawnables, player: &SequencePlayer) -> bool {
    spawnables.force_sync
        || player.clock == super::ClockSource::External
        || matches!(
            world.get_resource::<TimeUpdateStrategy>(),
            Some(TimeUpdateStrategy::ManualDuration(_))
        )
}

/// Whether a running player's spawnable still needs its prefab: no load started yet, a load in
/// flight, or a failure not reported to this player. A live edit
/// ([`SequencePlayer::replace_sequence`]) can add spawnables after the pre-roll ran.
fn needs_live_load(spawnables: Option<&SequenceSpawnables>, entity: Entity, spawnable: &Spawnable) -> bool {
    let Some(spawnables) = spawnables else {
        return true;
    };
    match spawnables.loads.get(&spawnable.prefab) {
        None => true,
        Some(PrefabLoad::Ready(_)) => false,
        Some(PrefabLoad::Failed(..)) => !spawnables
            .failed
            .get(&entity)
            .is_some_and(|f| f.contains(&spawnable.binding)),
        #[cfg(not(target_arch = "wasm32"))]
        Some(PrefabLoad::Loading(_)) => true,
    }
}

/// Marks and reports (once) every spawnable of `entity` whose prefab failed to load.
fn report_failed_loads(world: &mut World, spawnables: &mut SequenceSpawnables, entity: Entity) {
    let Some(mut player) = world.get_mut::<SequencePlayer>(entity) else {
        return;
    };
    let spawnable_list = player.sequence().spawnables.clone();
    for spawnable in &spawnable_list {
        if let Some(PrefabLoad::Failed(code, message)) = spawnables.loads.get(&spawnable.prefab) {
            spawnables
                .failed
                .entry(entity)
                .or_default()
                .insert(spawnable.binding.clone());
            if player.report(SequenceIssue::SpawnFailed {
                binding: spawnable.binding.clone(),
                prefab: spawnable.prefab.clone(),
                code: (*code).to_string(),
            }) {
                tracing::warn!(target: "regia_framework", binding = %spawnable.binding, "{message}");
            }
        }
    }
}

/// The pre-roll: loads the prefabs of every player waiting for them and ends its pre-roll once
/// each loaded or failed. Runs before the clocks advance, so a player whose loads finish this
/// frame starts this frame. A running player whose sequence gained spawnables by a live edit gets
/// their prefabs loaded too, without holding its clock: each actor appears once its prefab is
/// ready, and a failure is reported as at a start.
pub(super) fn preroll_spawnables(world: &mut World) {
    let mut query = world.query::<(Entity, &SequencePlayer)>();
    let mut waiting: Vec<(Entity, Vec<String>, bool, bool)> = Vec::new();
    let mut live: Vec<(Entity, Vec<String>, bool)> = Vec::new();
    {
        let spawnables = world.get_resource::<SequenceSpawnables>();
        for (entity, player) in query.iter(world) {
            if !player.is_prerolling() {
                if player.holds_outputs()
                    && player
                        .sequence()
                        .spawnables
                        .iter()
                        .any(|s| needs_live_load(spawnables, entity, s))
                {
                    let sync = spawnables.is_some_and(|s| sync_loads(world, s, player))
                        || (spawnables.is_none() && player.clock == super::ClockSource::External);
                    let prefabs = player
                        .sequence()
                        .spawnables
                        .iter()
                        .map(|s| s.prefab.clone())
                        .collect();
                    live.push((entity, prefabs, sync));
                }
                continue;
            }
            let sync = spawnables.is_some_and(|s| sync_loads(world, s, player))
                || (spawnables.is_none() && player.clock == super::ClockSource::External);
            let prefabs = player
                .sequence()
                .spawnables
                .iter()
                .map(|s| s.prefab.clone())
                .collect();
            waiting.push((entity, prefabs, sync, player.start_pending()));
        }
    }
    if waiting.is_empty() && live.is_empty() {
        return;
    }
    let _span = tracing::info_span!(target: "regia_framework", "sequencer_spawnable_preroll").entered();
    waiting.sort_unstable_by_key(|(entity, ..)| entity.to_bits());
    live.sort_unstable_by_key(|(entity, ..)| entity.to_bits());
    let mut spawnables = world.remove_resource::<SequenceSpawnables>().unwrap_or_default();
    for (entity, prefabs, sync) in live {
        for prefab in &prefabs {
            poll_load(world, &mut spawnables, prefab, sync);
        }
        report_failed_loads(world, &mut spawnables, entity);
    }
    for (entity, prefabs, sync, fresh) in waiting {
        if fresh {
            // A new start retries what failed last time (the file may have been fixed).
            for prefab in &prefabs {
                if matches!(spawnables.loads.get(prefab), Some(PrefabLoad::Failed(..))) {
                    spawnables.loads.remove(prefab);
                }
            }
            spawnables.failed.remove(&entity);
        }
        for prefab in &prefabs {
            poll_load(world, &mut spawnables, prefab, sync);
        }
        if prefabs.iter().any(|p| spawnables.is_loading(p)) {
            continue;
        }
        report_failed_loads(world, &mut spawnables, entity);
        if let Some(mut player) = world.get_mut::<SequencePlayer>(entity) {
            player.end_preroll();
        }
    }
    world.insert_resource(spawnables);
}

/// What the sync needs of one player.
struct PlayerView {
    entity: Entity,
    key: u128,
    holds: bool,
    stopped: bool,
    wanted: Vec<(String, Guid, bool)>,
    names: Vec<String>,
}

/// Spawns the actors whose range holds each player's clock and despawns the rest, then binds each
/// spawnable name for this frame's apply. Runs after the clocks advance.
pub(super) fn sync_spawnables(world: &mut World) {
    let _span = tracing::info_span!(target: "regia_framework", "sequencer_spawnables").entered();
    let mut spawnables = world.remove_resource::<SequenceSpawnables>().unwrap_or_default();
    let mut views: Vec<PlayerView> = Vec::new();
    {
        let mut query = world.query::<(Entity, &SequencePlayer, Option<&EntityGuid>)>();
        for (entity, player, guid) in query.iter(world) {
            let spawn = &player.sequence().spawnables;
            if spawn.is_empty() && !spawnables.live.contains_key(&entity) {
                continue;
            }
            let holds = player.holds_outputs() && !player.is_prerolling();
            let stopped = matches!(
                player.status(),
                PlaybackStatus::Stopped | PlaybackStatus::Finished
            ) && !holds;
            let (t, d) = (player.time(), player.duration());
            let failed = spawnables.failed.get(&entity);
            let wanted = if holds {
                spawn
                    .iter()
                    .filter(|s| s.live_at(t, d))
                    .filter(|s| !failed.is_some_and(|f| f.contains(&s.binding)))
                    .filter_map(|s| match spawnables.loads.get(&s.prefab) {
                        Some(PrefabLoad::Ready(guid)) => Some((s.binding.clone(), *guid, s.keep_on_stop)),
                        _ => None,
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let key = guid.map_or_else(
                || fnv128(&[b"regia.seq.player", player.sequence().name.as_bytes()]),
                |g| g.0.as_u128(),
            );
            views.push(PlayerView {
                entity,
                key,
                holds,
                stopped,
                wanted,
                names: player.spawn_names().to_vec(),
            });
        }
    }
    views.sort_unstable_by_key(|v| v.entity.to_bits());
    let mut changes: Vec<SpawnableChanged> = Vec::new();
    // Players that are gone take their actors with them (except the kept ones).
    let gone: Vec<Entity> = spawnables
        .live
        .keys()
        .copied()
        .filter(|p| !views.iter().any(|v| v.entity == *p))
        .collect();
    for player in gone {
        if let Some(actors) = spawnables.live.remove(&player) {
            for (binding, actor) in actors {
                retire(
                    world,
                    &mut spawnables.stats,
                    &mut changes,
                    player,
                    binding,
                    actor,
                    true,
                );
            }
        }
        spawnables.failed.remove(&player);
    }
    for view in &views {
        let live = spawnables.live.entry(view.entity).or_default();
        // An actor the game despawned is forgotten and not spawned again until the player stops.
        let lost: Vec<String> = live
            .iter()
            .filter(|(_, a)| world.get_entity(a.entity).is_err())
            .map(|(b, _)| b.clone())
            .collect();
        for binding in lost {
            live.remove(&binding);
            spawnables.failed.entry(view.entity).or_default().insert(binding);
        }
        // Despawns first, in binding order.
        let leaving: Vec<String> = live
            .keys()
            .filter(|b| !view.wanted.iter().any(|(w, ..)| w == *b))
            .cloned()
            .collect();
        for binding in leaving {
            if let Some(actor) = live.remove(&binding) {
                retire(
                    world,
                    &mut spawnables.stats,
                    &mut changes,
                    view.entity,
                    binding,
                    actor,
                    view.stopped,
                );
            }
        }
        // Then spawns, in the sequence's order.
        for (binding, prefab, keep) in &view.wanted {
            if live.contains_key(binding)
                || spawnables
                    .failed
                    .get(&view.entity)
                    .is_some_and(|f| f.contains(binding))
            {
                continue;
            }
            match spawn_actor(world, view.entity, view.key, binding, *prefab) {
                Ok((entity, guid)) => {
                    spawnables.stats.spawned += 1;
                    live.insert(
                        binding.clone(),
                        LiveActor {
                            entity,
                            guid,
                            keep_on_stop: *keep,
                        },
                    );
                    changes.push(SpawnableChanged {
                        player: view.entity,
                        binding: binding.clone(),
                        entity,
                        spawned: true,
                        kept: false,
                    });
                }
                Err(message) => {
                    spawnables
                        .failed
                        .entry(view.entity)
                        .or_default()
                        .insert(binding.clone());
                    if let Some(mut player) = world.get_mut::<SequencePlayer>(view.entity) {
                        let prefab_ref = player
                            .sequence()
                            .spawnables
                            .iter()
                            .find(|s| &s.binding == binding)
                            .map(|s| s.prefab.clone())
                            .unwrap_or_default();
                        if player.report(SequenceIssue::SpawnFailed {
                            binding: binding.clone(),
                            prefab: prefab_ref,
                            code: codes::PREFAB_SPAWN.to_string(),
                        }) {
                            tracing::warn!(target: "regia_framework", %binding, "{message}");
                        }
                    }
                }
            }
        }
        if !view.holds && view.stopped {
            // A stopped player retries failed spawns on its next start.
            spawnables.failed.remove(&view.entity);
        }
        let bound: Vec<(String, Option<Entity>)> = view
            .names
            .iter()
            .map(|name| (name.clone(), live.get(name).map(|a| a.entity)))
            .collect();
        if live.is_empty() {
            spawnables.live.remove(&view.entity);
        }
        if let Some(mut player) = world.get_mut::<SequencePlayer>(view.entity) {
            for (name, entity) in bound {
                player.bind_spawned(&name, entity);
            }
        }
    }
    world.insert_resource(spawnables);
    if !changes.is_empty() {
        for change in changes {
            world.write_message(change);
        }
    }
}

/// Despawns an actor, or leaves it to the game when `stopped` and it is `keep_on_stop`.
fn retire(
    world: &mut World,
    stats: &mut SpawnStats,
    changes: &mut Vec<SpawnableChanged>,
    player: Entity,
    binding: String,
    actor: LiveActor,
    stopped: bool,
) {
    let kept = stopped && actor.keep_on_stop;
    if kept {
        stats.kept += 1;
        if let Ok(mut entity) = world.get_entity_mut(actor.entity) {
            entity.remove::<SpawnedBySequence>();
        }
    } else {
        stats.despawned += 1;
        regia_scene::hierarchy::despawn_tree(world, actor.entity);
    }
    changes.push(SpawnableChanged {
        player,
        binding,
        entity: actor.entity,
        spawned: false,
        kept,
    });
}

/// Instantiates one actor under a fresh wrapper with a deterministic guid.
fn spawn_actor(
    world: &mut World,
    player: Entity,
    key: u128,
    binding: &str,
    prefab: Guid,
) -> Result<(Entity, Guid), String> {
    let taken: BTreeSet<u128> = {
        let mut guids = world.query::<&EntityGuid>();
        guids.iter(world).map(|g| g.0.as_u128()).collect()
    };
    let mut generation = 0u32;
    let mut guid = actor_guid(key, binding, generation);
    while taken.contains(&guid.as_u128()) && generation < u32::MAX {
        generation += 1;
        guid = actor_guid(key, binding, generation);
    }
    let root = world
        .spawn((
            EntityGuid(guid),
            Transform::default(),
            Name::new(binding.to_string()),
            SpawnedBySequence {
                player,
                binding: binding.to_string(),
            },
        ))
        .id();
    let instance = PrefabInstanceData::plain(SceneAssetRef(prefab));
    match instantiate_prefab(world, &instance, root, &SpawnOptions::default(), 0) {
        Ok(_) => Ok((root, guid)),
        Err(error) => {
            regia_scene::hierarchy::despawn_tree(world, root);
            Err(error.to_string())
        }
    }
}

/// Runs the spawn sync while a player exists or an actor is still live.
pub(super) fn spawn_wanted(
    players: Query<(), With<SequencePlayer>>,
    spawnables: Option<Res<SequenceSpawnables>>,
) -> bool {
    !players.is_empty() || spawnables.is_some_and(|s| !s.live.is_empty())
}
