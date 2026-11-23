// SPDX-License-Identifier: MPL-2.0
//! Motion databases as assets (`docs/specs/52-motion-matching.md` § 3, § 4; ADR-2409).
//!
//! A cooked `.motiondb.ron` is a `.fckd` file of type `regia_animation::MotionDatabase` whose
//! header lists its dependencies in a fixed order: the skeleton first, then one clip per source
//! entry in entry order (`regia_asset_pipeline::motiondb_cook`). [`load_motion_database_file`]
//! decodes the database and, through a reader for those ids, the skeleton and clips a
//! [`super::player::MotionMatchPlayer`] needs.
//!
//! [`MotionDatabases`] holds every database the app knows, by asset path: loaded from the asset
//! server ([`MotionDatabases::load_from_server`]) or put there by a host that builds one in code
//! ([`MotionDatabases::insert`]). The agent (`anim.motion_match_database { database: "<path>" }`)
//! and the editor's motion-matching panel read it.
//!
//! [`MotionDatabaseRef`] is the persisted reference a scene carries (`Reflect`, an
//! [`AssetRef`] plus an optional path). [`resolve_motion_database_refs`] (installed by
//! [`super::MotionMatchPlugin`]) turns it into a [`MotionMatching`] (and a player, when asked)
//! the first time the database is available, and again when it reloads.

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_asset::{AssetId, AssetRef, AssetServer};
use regia_ecs::prelude::*;

use super::player::MotionMatchPlayer;
use super::{MatcherSettings, MotionDatabase, MotionMatching};
use crate::clip::AnimationClip;
use crate::cooked::{
    MotionDatabaseLoadError, load_cooked_clip, load_cooked_motion_database, load_cooked_skeleton,
};
use crate::skeleton::Skeleton;

/// The type path a cooked motion database is stored under (its `.fckd` type tag hashes it).
pub const MOTION_DATABASE_TYPE_PATH: &str = "regia_animation::MotionDatabase";

/// The asset type an [`AssetRef`] to a motion database names (a marker: the loaded value is a
/// [`LoadedMotionDatabase`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Reflect, serde::Serialize, serde::Deserialize)]
pub struct MotionDatabaseAsset;

/// Why a motion database asset is not available. [`MotionDatabaseAssetError::code`] is the
/// agent's reason code.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum MotionDatabaseAssetError {
    /// No cooked asset at the path (or the id has no manifest entry).
    #[error("no cooked motion database at {0}")]
    NotFound(String),
    /// The cooked file could not be read.
    #[error("cannot read the cooked motion database: {0}")]
    Unreadable(String),
    /// The container or the payload failed its checks.
    #[error("{0}")]
    Corrupt(String),
    /// The payload is of a newer version.
    #[error("cooked motion database format_version {found} (supported {supported})")]
    UnsupportedVersion {
        /// The version in the file.
        found: u32,
        /// The newest this build reads.
        supported: u32,
    },
    /// A dependency (the skeleton or a clip) did not load.
    #[error("dependency {index} ({id}): {reason}")]
    Dependency {
        /// Index into the header's dependency list (0 = skeleton).
        index: usize,
        /// Its id.
        id: AssetId,
        /// What went wrong.
        reason: String,
    },
}

impl MotionDatabaseAssetError {
    /// The reason code: `unknown_database`, `database_unreadable`, `corrupt_database`,
    /// `unsupported_version` or `missing_dependency`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "unknown_database",
            Self::Unreadable(_) => "database_unreadable",
            Self::Corrupt(_) => "corrupt_database",
            Self::UnsupportedVersion { .. } => "unsupported_version",
            Self::Dependency { .. } => "missing_dependency",
        }
    }
}

impl From<MotionDatabaseLoadError> for MotionDatabaseAssetError {
    fn from(value: MotionDatabaseLoadError) -> Self {
        match value {
            MotionDatabaseLoadError::Database(super::MotionMatchError::UnsupportedFormatVersion {
                found,
                supported,
            }) => Self::UnsupportedVersion { found, supported },
            other => Self::Corrupt(other.to_string()),
        }
    }
}

/// A motion database with what plays it.
#[derive(Clone, Debug)]
pub struct LoadedMotionDatabase {
    /// The asset id (`None` for a database a host built in code).
    pub id: Option<AssetId>,
    /// The database.
    pub database: Arc<MotionDatabase>,
    /// The skeleton the clips animate (`None` when the host gave none).
    pub skeleton: Option<Arc<Skeleton>>,
    /// The clips in entry order ([`super::ClipRange::source`]); `None` when the host gave none.
    pub clips: Option<Arc<[AnimationClip]>>,
    /// Bumped every time the path is loaded or replaced.
    pub generation: u64,
}

impl LoadedMotionDatabase {
    /// A database a host built in code, with the skeleton and clips it was built from.
    pub fn built(database: MotionDatabase, skeleton: Skeleton, clips: Vec<AnimationClip>) -> Self {
        Self {
            id: None,
            database: Arc::new(database),
            skeleton: Some(Arc::new(skeleton)),
            clips: Some(clips.into()),
            generation: 0,
        }
    }

    /// A player over this database's clips, when it has them.
    pub fn player(&self) -> Option<MotionMatchPlayer> {
        Some(MotionMatchPlayer::new(
            self.clips.clone()?,
            self.skeleton.clone()?,
        ))
    }
}

/// Decode a cooked motion database file and its dependencies. `read` returns the whole cooked
/// file of an id; with `dependencies: false` only the database is decoded.
///
/// # Errors
///
/// [`MotionDatabaseAssetError::Corrupt`], [`MotionDatabaseAssetError::UnsupportedVersion`] or
/// [`MotionDatabaseAssetError::Dependency`].
#[tracing::instrument(target = "regia_animation", name = "motion_matching.load_asset", skip_all)]
pub fn load_motion_database_file(
    bytes: &[u8],
    dependencies: bool,
    mut read: impl FnMut(AssetId) -> Result<Vec<u8>, String>,
) -> Result<LoadedMotionDatabase, MotionDatabaseAssetError> {
    let (header, database) = load_cooked_motion_database(bytes)?;
    let mut loaded = LoadedMotionDatabase {
        id: Some(header.asset_id),
        database: Arc::new(database),
        skeleton: None,
        clips: None,
        generation: 0,
    };
    if !dependencies || header.dependencies.is_empty() {
        return Ok(loaded);
    }
    let fail = |index: usize, id: AssetId, reason: String| MotionDatabaseAssetError::Dependency {
        index,
        id,
        reason,
    };
    let entries = loaded
        .database
        .clips()
        .iter()
        .map(|c| c.source + 1)
        .max()
        .unwrap_or(0);
    if header.dependencies.len() != entries + 1 {
        return Err(MotionDatabaseAssetError::Corrupt(format!(
            "{} dependencies for {entries} entries (expected the skeleton and one clip per entry)",
            header.dependencies.len()
        )));
    }
    let mut ids = header.dependencies.iter().copied().enumerate();
    let Some((_, skeleton_id)) = ids.next() else {
        return Ok(loaded);
    };
    let skeleton_bytes = read(skeleton_id).map_err(|e| fail(0, skeleton_id, e))?;
    let (_, skeleton) =
        load_cooked_skeleton(&skeleton_bytes).map_err(|e| fail(0, skeleton_id, e.to_string()))?;
    let mut clips = Vec::with_capacity(entries);
    for (index, id) in ids {
        let clip_bytes = read(id).map_err(|e| fail(index, id, e))?;
        let (_, clip) = load_cooked_clip(&clip_bytes).map_err(|e| fail(index, id, e.to_string()))?;
        clips.push(clip);
    }
    loaded.skeleton = Some(Arc::new(skeleton));
    loaded.clips = Some(clips.into());
    Ok(loaded)
}

/// The cooked id of a project-relative path, with or without the `assets/` prefix.
fn cooked_id(server: &AssetServer, path: &str) -> Option<AssetId> {
    let rel = path.trim_start_matches("./");
    server
        .id_for_path(rel)
        .or_else(|| server.id_for_path(&format!("assets/{rel}")))
        .or_else(|| rel.strip_prefix("assets/").and_then(|r| server.id_for_path(r)))
}

/// Load the cooked motion database at `path` (project relative, `assets/` optional) with its
/// skeleton and clips, from the mounted paks or the cooked cache.
///
/// # Errors
///
/// [`MotionDatabaseAssetError::NotFound`], [`MotionDatabaseAssetError::Unreadable`] or the
/// errors of [`load_motion_database_file`].
pub fn load_motion_database(
    server: &mut AssetServer,
    path: &str,
) -> Result<LoadedMotionDatabase, MotionDatabaseAssetError> {
    let id = cooked_id(server, path).ok_or_else(|| MotionDatabaseAssetError::NotFound(path.to_owned()))?;
    load_motion_database_id(server, id)
}

/// [`load_motion_database`] by id.
///
/// # Errors
///
/// As [`load_motion_database`].
pub fn load_motion_database_id(
    server: &mut AssetServer,
    id: AssetId,
) -> Result<LoadedMotionDatabase, MotionDatabaseAssetError> {
    let (bytes, _) = server
        .read_cooked(id)
        .map_err(|e| MotionDatabaseAssetError::Unreadable(e.to_string()))?;
    server.track_external(id);
    load_motion_database_file(&bytes, true, |dep| {
        server.read_cooked(dep).map(|(b, _)| b).map_err(|e| e.to_string())
    })
}

/// Every motion database the app knows, by asset path, with the last load error per path.
#[derive(Resource, Debug, Default)]
pub struct MotionDatabases {
    loaded: BTreeMap<String, LoadedMotionDatabase>,
    errors: BTreeMap<String, MotionDatabaseAssetError>,
}

impl MotionDatabases {
    /// Put `database` at `path` (replacing what was there) and return its generation.
    pub fn insert(&mut self, path: impl Into<String>, mut database: LoadedMotionDatabase) -> u64 {
        let path = path.into();
        let generation = self.loaded.get(&path).map_or(1, |d| d.generation + 1);
        database.generation = generation;
        self.errors.remove(&path);
        self.loaded.insert(path, database);
        generation
    }

    /// The database at `path` (exact, then with or without `assets/`).
    pub fn get(&self, path: &str) -> Option<&LoadedMotionDatabase> {
        self.loaded
            .get(path)
            .or_else(|| self.loaded.get(&format!("assets/{path}")))
            .or_else(|| path.strip_prefix("assets/").and_then(|p| self.loaded.get(p)))
    }

    /// The path of a loaded database id.
    pub fn path_of(&self, id: AssetId) -> Option<&str> {
        self.loaded
            .iter()
            .find(|(_, d)| d.id == Some(id))
            .map(|(p, _)| p.as_str())
    }

    /// The path whose database is this `Arc` (the one an entity's [`MotionMatching`] shares).
    pub fn path_of_database(&self, database: &Arc<MotionDatabase>) -> Option<&str> {
        self.loaded
            .iter()
            .find(|(_, d)| Arc::ptr_eq(&d.database, database))
            .map(|(p, _)| p.as_str())
    }

    /// Every loaded database, sorted by path.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &LoadedMotionDatabase)> {
        self.loaded.iter().map(|(p, d)| (p.as_str(), d))
    }

    /// The last load error of `path`.
    pub fn error(&self, path: &str) -> Option<&MotionDatabaseAssetError> {
        self.errors.get(path)
    }

    /// Every recorded load error, sorted by path.
    pub fn errors(&self) -> impl Iterator<Item = (&str, &MotionDatabaseAssetError)> {
        self.errors.iter().map(|(p, e)| (p.as_str(), e))
    }

    /// Load `path` from the asset server into the registry (a failure is recorded and returned;
    /// a database already loaded at `path` stays in use).
    ///
    /// # Errors
    ///
    /// The errors of [`load_motion_database`].
    pub fn load_from_server(
        &mut self,
        server: &mut AssetServer,
        path: &str,
    ) -> Result<&LoadedMotionDatabase, MotionDatabaseAssetError> {
        match load_motion_database(server, path) {
            Ok(database) => {
                self.insert(path, database);
                self.get(path)
                    .ok_or_else(|| MotionDatabaseAssetError::NotFound(path.to_owned()))
            }
            Err(error) => {
                tracing::warn!(target: "regia_animation", path, code = error.code(), "motion database not loaded: {error}");
                self.errors.insert(path.to_owned(), error.clone());
                Err(error)
            }
        }
    }
}

/// A persisted reference from a character to its motion database: resolved by
/// [`resolve_motion_database_refs`] into a [`MotionMatching`] with default
/// [`MatcherSettings`] (and a [`MotionMatchPlayer`] when `player` is set and the database came
/// with its clips). `path`, when not empty, wins over `database`'s id; it may also name a
/// database a host put into [`MotionDatabases`] itself.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct MotionDatabaseRef {
    /// The cooked database by id.
    pub database: AssetRef<MotionDatabaseAsset>,
    /// The database by asset path (empty = use `database`).
    pub path: String,
    /// Also add a [`MotionMatchPlayer`] over the database's clips.
    pub player: bool,
}

impl MotionDatabaseRef {
    /// A reference by asset path, with a player.
    pub fn path(path: impl Into<String>) -> Self {
        Self {
            database: AssetRef::default(),
            path: path.into(),
            player: true,
        }
    }
}

/// What [`resolve_motion_database_refs`] last did for an entity's [`MotionDatabaseRef`].
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct MotionDatabaseRefState {
    /// The path the reference resolved to.
    pub path: Option<String>,
    /// The generation applied (a reload bumps it and the entity is updated again).
    pub generation: u64,
    /// The reason code of the last failure.
    pub error: Option<&'static str>,
}

/// What [`resolve_motion_database_refs`] reads per entity.
type RefQuery = (
    Entity,
    Ref<'static, MotionDatabaseRef>,
    Option<&'static mut MotionDatabaseRefState>,
    Option<&'static mut MotionMatching>,
    Has<MotionMatchPlayer>,
);

/// Resolve every [`MotionDatabaseRef`]: look the path (or the id's path) up in
/// [`MotionDatabases`], loading it from the [`AssetServer`] when there is one and it is not
/// loaded yet (a failed path is not retried until the reference changes), then insert or update
/// the entity's [`MotionMatching`] (and player). A database swap resets the matcher.
#[tracing::instrument(target = "regia_animation", skip_all)]
pub fn resolve_motion_database_refs(
    mut commands: Commands,
    databases: Option<ResMut<MotionDatabases>>,
    mut server: Option<ResMut<AssetServer>>,
    mut refs: Query<RefQuery>,
) {
    let Some(mut databases) = databases else {
        return;
    };
    for (entity, reference, state, matching, has_player) in &mut refs {
        let changed = reference.is_changed();
        let path = if reference.path.is_empty() {
            match server
                .as_deref()
                .and_then(|s| s.path_for_id(reference.database.id))
            {
                Some(path) => path.to_owned(),
                None => databases
                    .path_of(reference.database.id)
                    .map(str::to_owned)
                    .unwrap_or_else(|| reference.database.id.to_string()),
            }
        } else {
            reference.path.clone()
        };
        let current = state.as_deref().cloned().unwrap_or_default();
        let known = databases.get(&path).is_some();
        if !known && !changed && current.error.is_some() && current.path.as_deref() == Some(path.as_str()) {
            continue;
        }
        if !known && let Some(server) = server.as_deref_mut() {
            let _ = databases.load_from_server(server, &path);
        }
        let mut next = MotionDatabaseRefState {
            path: Some(path.clone()),
            generation: current.generation,
            error: None,
        };
        match databases.get(&path) {
            Some(loaded) => {
                if !changed
                    && current.path.as_deref() == Some(path.as_str())
                    && current.generation == loaded.generation
                {
                    continue;
                }
                next.generation = loaded.generation;
                match matching {
                    Some(mut matching) => {
                        matching.database = loaded.database.clone();
                        matching.base_database = None;
                        matching.matcher.reset();
                        matching.last_step = None;
                        matching.last_report = None;
                    }
                    None => {
                        commands.entity(entity).insert(MotionMatching::new(
                            loaded.database.clone(),
                            MatcherSettings::default(),
                        ));
                    }
                }
                if reference.player
                    && let Some(player) = loaded.player()
                    && (!has_player || current.generation != loaded.generation)
                {
                    commands.entity(entity).insert(player);
                }
            }
            None => {
                next.error = Some(
                    databases
                        .error(&path)
                        .map_or("unknown_database", MotionDatabaseAssetError::code),
                );
            }
        }
        match state {
            Some(mut state) => *state = next,
            None => {
                commands.entity(entity).insert(next);
            }
        }
    }
}
