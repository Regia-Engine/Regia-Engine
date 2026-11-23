// SPDX-License-Identifier: MPL-2.0
//! Opening a cooked streamed level (spec 78 §§ 6, 8.7).
//!
//! The level manager records the level and spawns its always-loaded set; its entities are level
//! members like any other, so unloading the level despawns them and the cells together.

use std::sync::Arc;

use regia_asset::AssetServer;
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_scene::Transform;
use regia_scene::format::SceneFile;
use regia_scene::level::{Level, LevelError, LevelId, LevelLibrary, LevelUnloading, LoadMode, open_level};
use regia_scene::spawn::{SceneSpawnError, SpawnOptions};
use regia_scene::streaming::{
    Readiness, StreamGates, StreamedOpen, StreamedReplicated, TERRAIN_COLLISION_GATE,
};
use thiserror::Error;

use crate::format::{
    WorldFormatError, WorldIndex, always_loaded_from_bytes, always_loaded_id, world_index_id,
};
use crate::origin::WorldOrigin;
use crate::runtime::{StreamRuntimeError, WorldStreaming};
use crate::source::{CellSource, PayloadCellSource};

/// Why a streamed level did not open.
#[derive(Debug, Error)]
pub enum StreamOpenError {
    /// The app has no asset server.
    #[error("the app has no asset server")]
    NoAssets,
    /// The app has no `WorldStreamingPlugin`.
    #[error("the app has no streaming runtime")]
    NoRuntime,
    /// The world index or the always-loaded set did not read.
    #[error(transparent)]
    Format(#[from] WorldFormatError),
    /// The level manager refused.
    #[error(transparent)]
    Level(#[from] LevelError),
    /// The always-loaded set did not spawn.
    #[error(transparent)]
    Spawn(#[from] SceneSpawnError),
    /// The runtime refused the index.
    #[error(transparent)]
    Runtime(#[from] StreamRuntimeError),
    /// No cooked world index: the level was never cooked, or its last cook stopped before it
    /// wrote the index (the cook removes the old one first, ADR-8812). In a packaged game whose
    /// content chunk holding the index is not mounted, `chunk` names that chunk: the fix is to
    /// request and mount it, not to cook again (ADR-8862).
    #[error("{}", not_cooked_message(chunk.as_deref()))]
    NotCooked {
        /// The content chunk the chunk map homes the level's world index in, when that chunk is
        /// not mounted.
        chunk: Option<String>,
    },
}

/// The message of [`StreamOpenError::NotCooked`].
fn not_cooked_message(chunk: Option<&str>) -> String {
    match chunk {
        Some(chunk) => format!(
            "the level's world index is in content chunk `{chunk}`, which is not mounted; request and \
             mount that chunk (content.request, content.mount) instead of cooking again"
        ),
        None => {
            "the level has no cooked world index (not cooked, or its last cook did not finish); cook it again"
                .to_owned()
        }
    }
}

impl StreamOpenError {
    /// The stable code, where the agent protocol has one.
    ///
    /// `stream_not_enabled` is only "nothing is streaming" (no runtime, or the level manager
    /// refused). A bad index, a decode failure, a missing asset server and a spawn failure each
    /// have their own code (`book/agents/streaming-runtime.md`).
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Format(WorldFormatError::Newer { .. }) => "stream_format_newer",
            Self::Format(WorldFormatError::Corrupt(message)) if message.starts_with("settings:") => {
                "stream_settings_invalid"
            }
            Self::Format(WorldFormatError::Corrupt(_)) => "stream_index_corrupt",
            Self::Format(WorldFormatError::Decode(_)) => "stream_index_decode",
            Self::Spawn(_) => "stream_spawn_failed",
            Self::NoAssets => "stream_no_assets",
            Self::NotCooked { .. } => "stream_not_cooked",
            Self::Runtime(error) => error.reason(),
            Self::NoRuntime | Self::Level(_) => "stream_not_enabled",
        }
    }

    /// The not-mounted content chunk a [`StreamOpenError::NotCooked`] names, if any.
    #[must_use]
    pub fn unmounted_chunk(&self) -> Option<&str> {
        match self {
            Self::NotCooked { chunk } => chunk.as_deref(),
            _ => None,
        }
    }
}

/// Opens a cooked streamed level from the mounted paks and cooked cache. The index and the
/// always-loaded set are read here, as part of the level open; every cell after that streams on
/// worker threads. Returns the level's always-loaded entity count.
///
/// A world index newer than this build still opens the always-loaded set and then returns
/// [`StreamOpenError`] `stream_format_newer` (spec 78 § 6.5). Cell streaming does not start.
pub fn open_streamed_level(
    world: &mut World,
    level: LevelId,
    mode: LoadMode,
) -> Result<usize, StreamOpenError> {
    let assets = world
        .get_resource::<AssetServer>()
        .ok_or(StreamOpenError::NoAssets)?;
    let reader = assets.payload_reader();
    let (index_bytes, _) = reader.read(world_index_id(level)).map_err(|error| match error {
        regia_asset::AssetLoadError::Cooked(regia_asset::CookedIoError::Io(io))
            if io.kind() == std::io::ErrorKind::NotFound =>
        {
            // A packaged game: the chunk map may home the index in a chunk that is not mounted.
            StreamOpenError::NotCooked {
                chunk: assets.unavailable_chunk(world_index_id(level)).map(str::to_owned),
            }
        }
        error => WorldFormatError::from(error).into(),
    })?;
    let index = match WorldIndex::from_bytes(&index_bytes, level) {
        Ok(index) => index,
        // Spec 78 § 6.5: a newer index is refused, and the level still opens with the
        // always-loaded set. Streaming does not start. If that set itself cannot be read, the
        // level stays closed and the error is the read's.
        Err(WorldFormatError::Newer { found, supported }) => {
            let (always_bytes, _) = reader
                .read(always_loaded_id(level))
                .map_err(WorldFormatError::from)?;
            let always = always_loaded_from_bytes(&always_bytes, level)?;
            return open_always_loaded_only(world, level, mode, &always, found, supported);
        }
        Err(error) => return Err(error.into()),
    };
    let (always_bytes, _) = reader
        .read(always_loaded_id(level))
        .map_err(WorldFormatError::from)?;
    let always = always_loaded_from_bytes(&always_bytes, level)?;
    // The scatter's voxel proxies (ADR-3253): a sidecar the cook writes only for a level with
    // scatter. A table that does not read leaves the level without them, never unopened.
    let voxels = match reader.read(crate::hlod_voxels::hlod_voxels_id(level)) {
        Ok((bytes, _)) => match crate::hlod_voxels::HlodVoxels::from_bytes(&bytes, level) {
            Ok(table) => Some(table),
            Err(error) => {
                tracing::warn!(target: "regia_world", "HLOD voxel table of {}: {error}; drawn without", level.0);
                None
            }
        },
        // No sidecar is the normal case for a level without scatter; any other failure is said once.
        Err(regia_asset::AssetLoadError::Cooked(regia_asset::CookedIoError::Io(io)))
            if io.kind() == std::io::ErrorKind::NotFound =>
        {
            None
        }
        Err(error) => {
            tracing::warn!(target: "regia_world", "HLOD voxel table of {}: {error}; drawn without", level.0);
            None
        }
    };
    let source: Arc<dyn CellSource> = Arc::new(PayloadCellSource::new(reader));
    let spawned = open_streamed_from(world, level, mode, index, &always, source)?;
    if let Some(table) = voxels
        && let Some(mut streaming) = world.get_resource_mut::<WorldStreaming>()
        && let Err(error) = streaming.set_hlod_voxels(level, table)
    {
        tracing::warn!(target: "regia_world", "{error}; the level draws without voxel proxies");
    }
    Ok(spawned)
}

/// Opens `level` through the runtime when it has a cooked world index, and tells the caller to open
/// it the ordinary way when it has none ([`regia_scene::streaming::StreamedLevelOpener`]).
pub fn open_if_cooked(world: &mut World, level: LevelId, mode: LoadMode) -> StreamedOpen {
    let cooked = world
        .get_resource::<AssetServer>()
        .is_some_and(|assets| assets.payload_reader().read(world_index_id(level)).is_ok());
    if !cooked {
        return StreamedOpen::NotCooked;
    }
    match open_streamed_level(world, level, mode) {
        Ok(_) => StreamedOpen::Opened,
        Err(StreamOpenError::Format(WorldFormatError::Newer { .. })) => StreamedOpen::AlwaysLoaded,
        Err(error) => StreamedOpen::Failed(error.to_string()),
    }
}

/// Spec 78 § 6.5: the index is newer than this build. Open the level with the always-loaded set,
/// do not start cell streaming, say so once, and still return `stream_format_newer`.
fn open_always_loaded_only(
    world: &mut World,
    level: LevelId,
    mode: LoadMode,
    always: &regia_scene::cooked::CookedScene,
    found: u32,
    supported: u32,
) -> Result<usize, StreamOpenError> {
    let empty = Level::from_scene_file(level, SceneFile::default());
    world.get_resource_or_insert_with(LevelLibrary::new).insert(empty);
    open_level(world, level, mode)?;
    let spawned = always.spawn(
        world,
        &SpawnOptions {
            level: Some(level),
            ..SpawnOptions::default()
        },
    )?;
    for warning in &spawned.warnings {
        tracing::warn!(target: "regia_world", "level {level}: {warning}");
    }
    tracing::warn!(
        target: "regia_world",
        "stream_format_newer: world index format version {found} is newer than {supported}; \
         level {level} opened with its always-loaded set only"
    );
    Err(StreamOpenError::Format(WorldFormatError::Newer {
        found,
        supported,
    }))
}

/// The origin cell on a grid of `cell_size` meters that keeps local space where `old` put it. A level
/// with another origin cell size can open after a rebase only if the offset falls on its grid: local
/// space must not move under what already lives in it.
fn same_offset_on(old: &WorldOrigin, cell_size: f64) -> Result<[i64; 3], StreamRuntimeError> {
    let from = old.0.cell();
    let old_size = old.0.space().cell_size();
    if from == [0; 3] || old_size == cell_size {
        return Ok(from);
    }
    let mut cell = [0_i64; 3];
    for (to, at) in cell.iter_mut().zip(from) {
        let on_grid = at as f64 * old_size / cell_size;
        if !on_grid.is_finite() || on_grid.fract() != 0.0 {
            return Err(StreamRuntimeError::BadIndex(format!(
                "origin: local space sits at cell {from:?} of {old_size} m, which is not on this level's \
                 {cell_size} m origin grid; close the streamed levels first so the origin goes home"
            )));
        }
        *to = on_grid as i64;
    }
    Ok(cell)
}

/// [`open_streamed_level`] with the cooked data in hand and any cell source.
pub fn open_streamed_from(
    world: &mut World,
    level: LevelId,
    mode: LoadMode,
    index: WorldIndex,
    always: &regia_scene::cooked::CookedScene,
    source: Arc<dyn CellSource>,
) -> Result<usize, StreamOpenError> {
    if !world.contains_resource::<WorldStreaming>() {
        return Err(StreamOpenError::NoRuntime);
    }
    // A settings failure is `stream_settings_invalid`, any other bad index `stream_index_corrupt`
    // (`StreamOpenError::reason`); neither reads as "streaming is off".
    index.validate()?;
    // Local space belongs to the process: whatever already lives in it (persistent entities, a level
    // opened additively) keeps its origin cell.
    let cell = match world.get_resource::<WorldOrigin>() {
        Some(old) => same_offset_on(old, index.settings.origin.cell_size_m)?,
        None => [0; 3],
    };
    let origin = WorldOrigin::at_cell(&index.settings.origin, cell)
        .map_err(|e| StreamRuntimeError::BadIndex(format!("origin: {e}")))?;

    // The manager's record: an empty level whose content arrives from the cooked data below.
    let empty = Level::from_scene_file(level, SceneFile::default());
    world.get_resource_or_insert_with(LevelLibrary::new).insert(empty);
    open_level(world, level, mode)?;
    let spawned = always.spawn(
        world,
        &SpawnOptions {
            level: Some(level),
            ..SpawnOptions::default()
        },
    )?;
    for warning in &spawned.warnings {
        tracing::warn!(target: "regia_world", "level {level}: {warning}");
    }
    // The set is cooked in world coordinates; local space may have moved since the first open.
    if cell != [0; 3] {
        for root in &spawned.roots {
            if let Some(mut transform) = world.get_mut::<Transform>(*root) {
                let at = transform.translation;
                let local = origin
                    .from_world([f64::from(at.x), f64::from(at.y), f64::from(at.z)])
                    .map_err(|e| StreamRuntimeError::BadIndex(format!("origin: {e}")))?;
                transform.translation = Vec3::from_array(local);
            }
        }
    }
    // A level with terrain waits for its collider; other levels never see this gate.
    if index.cells.iter().any(|c| c.has_terrain_collision) {
        let mut gates = world.get_resource_or_init::<StreamGates>();
        let gate = gates.register_for_claimed_levels(TERRAIN_COLLISION_GATE, Readiness::SimReady);
        gates.claim_level(gate, level);
    }
    world.insert_resource(index.settings.clone());
    world.insert_resource(origin);
    // The nav hook builds its streamed tile cache from this.
    if let Some(layout) = index.nav {
        world.insert_resource(layout);
    }
    // A net client keeps binds for these until their cells spawn (spec 78 § 14, spec 71 § 4.5).
    let replicated: Vec<(u128, regia_scene::stream_grid::CellCoord)> = index
        .replicated
        .iter()
        .map(|(guid, cell)| (guid.as_u128(), *cell))
        .collect();
    world
        .resource_mut::<WorldStreaming>()
        .open_level(level, index, source)?;
    world
        .get_resource_or_init::<StreamedReplicated>()
        .insert_level(level, replicated);
    if let Some(mut active) = world.get_resource_mut::<regia_scene::streaming::StreamingActive>() {
        active.set_if_neq(regia_scene::streaming::StreamingActive(true));
    }
    Ok(spawned.entities)
}

/// `OnLevelUnloading`: forgets the replicated-entity table of every streamed level that unloads, so
/// a net client stops keeping binds for it (spec 78 § 14).
pub fn forget_replicated_tables(
    mut unloading: MessageReader<LevelUnloading>,
    table: Option<ResMut<StreamedReplicated>>,
) {
    let Some(mut table) = table else {
        return;
    };
    for message in unloading.read() {
        table.remove_level(message.level);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::id::Guid;

    fn world_with_server(tag: &str) -> (World, std::path::PathBuf) {
        let root = regia_test_utils::unique_temp_dir(tag);
        let server = AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("asset server");
        let mut world = World::new();
        world.insert_resource(server);
        (world, root)
    }

    /// ADR-8862 (review w7 finding 6): a level whose world index the chunk map homes in a content
    /// chunk that is not mounted says so and names the chunk, instead of "cook it again".
    #[test]
    fn a_level_in_an_unmounted_chunk_names_the_chunk_instead_of_cook_again() {
        let level = LevelId(Guid::from_u128(0x8862));
        let (mut world, root) = world_with_server("world-open-unmounted-chunk");
        let mut homes = std::collections::BTreeMap::new();
        homes.insert(world_index_id(level), "forest".to_owned());
        world
            .resource_mut::<AssetServer>()
            .set_chunk_map(Some(regia_asset::content::ChunkMap::from_assignment(&homes)));
        let error = open_streamed_level(&mut world, level, LoadMode::Additive).expect_err("not mounted");
        assert_eq!(error.reason(), "stream_not_cooked");
        assert_eq!(error.unmounted_chunk(), Some("forest"));
        let text = error.to_string();
        assert!(
            text.contains("content chunk `forest`") && text.contains("not mounted"),
            "{text}"
        );
        assert!(!text.contains("cook it again"), "{text}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// ADR-8862: with no chunk map (a development run), the hint is still "cook it again".
    #[test]
    fn a_level_never_cooked_still_says_cook_it_again() {
        let level = LevelId(Guid::from_u128(0x8863));
        let (mut world, root) = world_with_server("world-open-never-cooked");
        let error = open_streamed_level(&mut world, level, LoadMode::Additive).expect_err("not cooked");
        assert_eq!(error.reason(), "stream_not_cooked");
        assert_eq!(error.unmounted_chunk(), None);
        assert!(error.to_string().contains("cook it again"), "{error}");
        std::fs::remove_dir_all(&root).ok();
    }
}
