// SPDX-License-Identifier: MPL-2.0
//! Loading `.camera.ron` files and reloading them when they change
//! (`docs/specs/35-camera-framework.md` § 3, ADR-2030).
//!
//! [`load_camera_file`] reads an asset path through the same route as the other data tables
//! ([`crate::table_source`]): the source text when there is one, else the cooked pak entry. The
//! asset lands in [`CameraAssets`] under the name written in the file, and [`CameraFiles`] remembers
//! which file it came from, what the last read did and why it failed.
//!
//! A changed file ([`SourceAssetChanged`]) is read again. A file that does not parse changes nothing
//! and records the error; one with validation errors becomes the working copy while drivers stay on
//! the last good version (the registry's rule). A good one gets a new revision, and every
//! [`CameraDriver`] running it rebinds on its next frame, keeping its variables and parameter
//! overrides.
//!
//! [`CameraAssetSource`] is the level's way to name a camera: it is reflected, so a level and a
//! play-in-editor snapshot carry it where the non-reflected [`CameraDriver`] cannot be.

use std::collections::BTreeMap;
use std::path::{Component as PathComponent, Path};

use regia_app::prelude::*;
use regia_asset::SourceAssetChanged;
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_scene::components::Name;

use super::asset::{CAMERA_ASSET_TYPE_PATH, CameraAsset, CameraBuildStatus};
use super::director::CameraHooks;
use super::driver::{CameraAssets, CameraDriver};
use crate::table_source::{TableBytes, read_table};

/// Agent code: the path is not a plain `.camera.ron` path under `assets/`.
pub const BAD_PATH: &str = "camera_bad_path";
/// Agent code: neither source text nor a cooked entry exists at the path.
pub const FILE_NOT_FOUND: &str = "camera_file_not_found";

/// Why a camera file did not load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraFileError {
    /// A stable agent code: [`BAD_PATH`], [`FILE_NOT_FOUND`] or `camera_asset_invalid`.
    pub reason: &'static str,
    /// What went wrong; a parse error carries the line and column.
    pub detail: String,
}

impl std::fmt::Display for CameraFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason, self.detail)
    }
}

/// One `.camera.ron` this world has read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CameraFile {
    /// The name of the asset in [`CameraAssets`], once the file has parsed.
    pub asset: Option<String>,
    /// The text came from a cooked entry, not from the source file.
    pub cooked: bool,
    /// How many times the file has been read and parsed.
    pub generation: u64,
    /// The last failure, cleared by a read that parses.
    pub error: Option<CameraFileError>,
}

/// The camera files of this world, by asset path.
#[derive(Resource, Debug, Default)]
pub struct CameraFiles {
    files: BTreeMap<String, CameraFile>,
}

impl CameraFiles {
    /// The record of an asset path.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&CameraFile> {
        self.files.get(path)
    }

    /// Every file, sorted by path.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &CameraFile)> {
        self.files.iter().map(|(path, file)| (path.as_str(), file))
    }

    /// The path of the file `asset` came from.
    #[must_use]
    pub fn path_of_asset(&self, asset: &str) -> Option<&str> {
        self.iter()
            .find(|(_, file)| file.asset.as_deref() == Some(asset))
            .map(|(path, _)| path)
    }
}

/// `path` as an asset path: forward slashes, no leading `assets/`.
///
/// # Errors
/// [`BAD_PATH`] for an empty or absolute path, one that climbs (`..`), or a file name that does not
/// end in `.camera.ron`.
pub fn normalize_camera_path(path: &str) -> Result<String, CameraFileError> {
    let bad = |why: &str| CameraFileError {
        reason: BAD_PATH,
        detail: format!("{path:?} {why}"),
    };
    let unified = path.replace('\\', "/");
    let plain = Path::new(&unified)
        .components()
        .all(|part| matches!(part, PathComponent::Normal(_) | PathComponent::CurDir));
    if unified.is_empty() || !plain {
        return Err(bad("must be a plain path relative to assets/"));
    }
    let rel = unified.strip_prefix("./").unwrap_or(&unified);
    let rel = rel.strip_prefix("assets/").unwrap_or(rel);
    if !is_camera_file_name(rel) {
        return Err(bad("must name a .camera.ron file"));
    }
    Ok(rel.to_owned())
}

fn is_camera_file_name(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".camera.ron")
}

/// Stores `text` as the asset of file `path` (not normalized: the key is used as given) and returns
/// the asset's build status.
///
/// # Errors
/// `camera_asset_invalid` when the text does not parse or is a newer version; what was loaded
/// before stays in use and [`CameraFile::error`] says why.
pub fn load_camera_text(
    world: &mut World,
    path: &str,
    text: &str,
    cooked: bool,
) -> Result<CameraBuildStatus, CameraFileError> {
    let _span = tracing::debug_span!(target: "regia_framework", "camera_file_load").entered();
    let parsed = CameraAsset::from_ron(text);
    let mut files = world.get_resource_or_init::<CameraFiles>();
    let file = files.files.entry(path.to_owned()).or_default();
    let asset = match parsed {
        Ok(asset) => {
            file.asset = Some(asset.name.clone());
            file.cooked = cooked;
            file.generation += 1;
            file.error = None;
            asset
        }
        Err(error) => {
            let failure = CameraFileError {
                reason: error.code(),
                detail: error.to_string(),
            };
            tracing::warn!(target: "regia_framework", path, "camera file not loaded: {failure}");
            file.error = Some(failure.clone());
            return Err(failure);
        }
    };
    world.get_resource_or_init::<CameraHooks>();
    world.get_resource_or_init::<CameraAssets>();
    let status = world.resource_scope(|world, mut assets: Mut<CameraAssets>| {
        assets.insert(asset, Some(path.to_owned()), world.resource::<CameraHooks>())
    });
    Ok(status)
}

/// Reads the asset path `path` (source text, else its cooked entry) into [`CameraAssets`].
///
/// # Errors
/// [`BAD_PATH`], [`FILE_NOT_FOUND`], or `camera_asset_invalid` (see [`load_camera_text`]). A failure
/// is also recorded in [`CameraFiles`] so a level naming the file does not read it every frame.
pub fn load_camera_file(world: &mut World, path: &str) -> Result<CameraBuildStatus, CameraFileError> {
    let path = normalize_camera_path(path)?;
    match read_table(world, &path, CAMERA_ASSET_TYPE_PATH) {
        Ok(TableBytes::Text(text)) => load_camera_text(world, &path, &text, false),
        Ok(TableBytes::Cooked(bytes)) => match String::from_utf8(bytes) {
            Ok(text) => load_camera_text(world, &path, &text, true),
            Err(_) => Err(record_failure(
                world,
                &path,
                "camera_asset_invalid",
                "the cooked entry is not UTF-8".to_owned(),
            )),
        },
        Err(detail) => Err(record_failure(world, &path, FILE_NOT_FOUND, detail)),
    }
}

fn record_failure(world: &mut World, path: &str, reason: &'static str, detail: String) -> CameraFileError {
    let failure = CameraFileError { reason, detail };
    tracing::warn!(target: "regia_framework", path, "camera file not loaded: {failure}");
    let mut files = world.get_resource_or_init::<CameraFiles>();
    files.files.entry(path.to_owned()).or_default().error = Some(failure.clone());
    failure
}

/// The key to reload for a changed `file`: the longest known path the file ends with, as chooser and
/// curve table reload route (ADR-1809). Assets a host stored with a path count too.
fn owner_of(files: &CameraFiles, assets: Option<&CameraAssets>, file: &Path) -> Option<String> {
    let known = files.files.keys().map(String::as_str);
    let hosted = assets
        .into_iter()
        .flat_map(|assets| assets.names().filter_map(|name| assets.path(name)));
    known
        .chain(hosted)
        .filter(|path| file.ends_with(Path::new(path)))
        .max_by_key(|path| path.len())
        .map(str::to_owned)
}

/// `Update`: reads the camera files that changed on disk. Takes no allocation when nothing changed.
#[tracing::instrument(skip_all, target = "regia_framework", name = "camera_files_reload")]
fn reload_changed_camera_files(world: &mut World, mut changes: Local<MessageCursor<SourceAssetChanged>>) {
    let mut reload: Vec<(String, std::path::PathBuf)> = Vec::new();
    if let (Some(messages), Some(files)) = (
        world.get_resource::<Messages<SourceAssetChanged>>(),
        world.get_resource::<CameraFiles>(),
    ) {
        let assets = world.get_resource::<CameraAssets>();
        for change in changes.read(messages) {
            let is_camera = change
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_camera_file_name);
            if !is_camera {
                continue;
            }
            if let Some(path) = owner_of(files, assets, &change.path)
                && !reload.iter().any(|(known, _)| *known == path)
            {
                reload.push((path, change.path.clone()));
            }
        }
    }
    for (path, file) in reload {
        // The message names the file on disk, so this does not depend on the asset server's root.
        match std::fs::read_to_string(&file) {
            // A refused file has already been recorded, with what stays in use.
            Ok(text) => {
                if let Ok(status) = load_camera_text(world, &path, &text, false) {
                    tracing::info!(target: "regia_framework", path, status = status.name(), "camera file reloaded");
                }
            }
            Err(error) => {
                record_failure(
                    world,
                    &path,
                    FILE_NOT_FOUND,
                    format!("{}: {error}", file.display()),
                );
            }
        }
    }
}

/// The registry, the file records and hot reload, without any driver: what the editor's edit world
/// needs to list and validate camera assets while no camera moves. [`super::CameraPlugin`] adds it.
pub struct CameraAssetsPlugin;

impl Plugin for CameraAssetsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CameraAssets>()
            .init_resource::<CameraHooks>()
            .init_resource::<CameraFiles>()
            .add_message::<SourceAssetChanged>()
            .register_type::<CameraAssetSource>()
            .add_systems(Update, reload_changed_camera_files);
    }
}

/// Names the camera asset of this camera entity by file, from a level. The driver system's frame
/// loads the file when this world has not read it and attaches or retargets a [`CameraDriver`].
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct CameraAssetSource {
    /// The `.camera.ron`, relative to `assets/`.
    pub path: String,
    /// The `Name` of the entity the driver follows; empty for none.
    pub target: String,
    /// Seed of the deterministic noise (shake nodes).
    pub seed: u64,
}

impl CameraAssetSource {
    /// A source for the file `path`, with no target.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            ..Self::default()
        }
    }
}

/// `Update`, in play worlds: gives each [`CameraAssetSource`] its [`CameraDriver`]. A file that failed
/// is not read again until it changes.
#[tracing::instrument(skip_all, target = "regia_framework", name = "camera_sources_bind")]
pub(super) fn bind_camera_sources(
    world: &mut World,
    query: &mut QueryState<(Entity, &CameraAssetSource, Option<&CameraDriver>)>,
    mut todo: Local<Vec<(Entity, CameraAssetSource)>>,
) {
    todo.clear();
    let files = world.get_resource::<CameraFiles>();
    for (entity, source, driver) in query.iter(world) {
        let Ok(path) = normalize_camera_path(&source.path) else {
            continue;
        };
        let file = files.and_then(|files| files.get(&path));
        let due = match (file, driver) {
            (None, _) => true,
            (Some(file), None) => file.asset.is_some(),
            (Some(file), Some(driver)) => {
                file.asset.as_deref().is_some_and(|name| name != driver.asset)
                    || (driver.target.is_none() && !source.target.is_empty())
            }
        };
        if due {
            todo.push((entity, source.clone()));
        }
    }
    let due = std::mem::take(&mut *todo);
    for (entity, source) in &due {
        let Ok(path) = normalize_camera_path(&source.path) else {
            continue;
        };
        let known = world
            .get_resource::<CameraFiles>()
            .is_some_and(|files| files.get(&path).is_some());
        if !known {
            // A failure is recorded in `CameraFiles` and reported by the agent's `camera.assets`.
            let _ = load_camera_file(world, &path);
        }
        let name = world
            .get_resource::<CameraFiles>()
            .and_then(|files| files.get(&path))
            .and_then(|file| file.asset.clone());
        let Some(name) = name else {
            continue;
        };
        let target = named_entity(world, &source.target);
        let Some(mut driver_entity) = world.get_entity_mut(*entity).ok() else {
            continue;
        };
        if let Some(mut driver) = driver_entity.get_mut::<CameraDriver>() {
            if driver.asset != name {
                driver.asset = name;
                driver.unbind();
            }
            driver.target = driver.target.or(target);
        } else {
            let mut driver = CameraDriver::new(name).with_seed(source.seed);
            driver.target = target;
            driver_entity.insert(driver);
        }
    }
    *todo = due;
}

fn named_entity(world: &mut World, wanted: &str) -> Option<Entity> {
    if wanted.is_empty() {
        return None;
    }
    world
        .query::<(Entity, &Name)>()
        .iter(world)
        .find(|(_, name)| name.as_str() == wanted)
        .map(|(entity, _)| entity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_paths_are_normalized_and_stay_inside_assets() {
        for (given, want) in [
            ("camera/a.camera.ron", "camera/a.camera.ron"),
            ("assets/camera/a.camera.ron", "camera/a.camera.ron"),
            ("./a.camera.ron", "a.camera.ron"),
            ("cam\\b.CAMERA.RON", "cam/b.CAMERA.RON"),
        ] {
            assert_eq!(normalize_camera_path(given).as_deref(), Ok(want), "{given}");
        }
        for bad in [
            "",
            "../a.camera.ron",
            "/etc/a.camera.ron",
            "a/../../b.camera.ron",
            "a.ron",
        ] {
            assert_eq!(
                normalize_camera_path(bad).map_err(|e| e.reason),
                Err(BAD_PATH),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_longest_known_path_owns_a_changed_file() {
        let mut files = CameraFiles::default();
        files.files.insert("a.camera.ron".into(), CameraFile::default());
        files
            .files
            .insert("cam/a.camera.ron".into(), CameraFile::default());
        let owner = owner_of(&files, None, Path::new("/p/assets/cam/a.camera.ron"));
        assert_eq!(owner.as_deref(), Some("cam/a.camera.ron"));
        let owner = owner_of(&files, None, Path::new("/p/assets/a.camera.ron"));
        assert_eq!(owner.as_deref(), Some("a.camera.ron"));
        assert_eq!(owner_of(&files, None, Path::new("/p/assets/b.camera.ron")), None);
    }
}
