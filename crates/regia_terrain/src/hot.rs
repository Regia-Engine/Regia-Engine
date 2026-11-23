// SPDX-License-Identifier: MPL-2.0
//! Live reload of terrain layer stacks and scatter rules (`docs/specs/12-assets.md` § 8).
//!
//! Both formats are cooked (the tiled terrain path and the cooked scatter set), and the cooked
//! tiles reload through the asset server. What the runtime parses from the *source* lives here:
//!
//! - a [`Terrain`] entity with a [`TerrainLayersSource`] gets its [`Terrain::layers`] re-read
//!   when the `.terrain.layers.ron` changes (change detection tells the host to re-upload);
//! - [`ScatterSets`] holds parsed `.scatter.ron` rules by path and replaces an entry when its
//!   file changes ([`ScatterSets::revision`] tells the host to re-place).
//!
//! A file that does not parse keeps the previous data and is reported.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_app::prelude::*;

use crate::Terrain;
use crate::layers::TerrainLayers;
use crate::scatter::ScatterSet;

/// The `.terrain.layers.ron` a [`Terrain`]'s layer stack comes from.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub struct TerrainLayersSource(pub PathBuf);

/// Parsed scatter rule sets by file path.
#[derive(Resource, Default)]
pub struct ScatterSets {
    sets: HashMap<PathBuf, (Arc<ScatterSet>, u64)>,
}

/// The cache key of a file: its canonical path. A file that is gone cannot be canonicalized, so
/// its parent directory is (a symlinked temp dir such as macOS `/var` must key the same before
/// and after the file is deleted, or the reload of a deleted file finds no entry).
fn key(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    match (
        path.parent().and_then(|p| p.canonicalize().ok()),
        path.file_name(),
    ) {
        (Some(parent), Some(name)) => parent.join(name),
        _ => path.to_path_buf(),
    }
}

fn read_scatter(path: &Path) -> Result<ScatterSet, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    ScatterSet::from_ron(&text, None).map_err(|e| e.to_string())
}

impl ScatterSets {
    /// The set at `path`, parsed on first use.
    ///
    /// # Errors
    ///
    /// The file could not be read or is not a valid scatter set.
    pub fn load(&mut self, path: impl AsRef<Path>) -> Result<Arc<ScatterSet>, String> {
        let key = key(path.as_ref());
        if let Some((set, _)) = self.sets.get(&key) {
            return Ok(Arc::clone(set));
        }
        let set = Arc::new(read_scatter(&key)?);
        self.sets.insert(key, (Arc::clone(&set), 0));
        Ok(set)
    }

    /// The loaded set at `path`.
    #[must_use]
    pub fn get(&self, path: impl AsRef<Path>) -> Option<&Arc<ScatterSet>> {
        self.sets.get(&key(path.as_ref())).map(|(set, _)| set)
    }

    /// Reload count of the set at `path` (`None` when not loaded).
    #[must_use]
    pub fn revision(&self, path: impl AsRef<Path>) -> Option<u64> {
        self.sets.get(&key(path.as_ref())).map(|(_, rev)| *rev)
    }

    /// Re-parses `path` if it is loaded; `Ok(false)` when it is not.
    ///
    /// # Errors
    ///
    /// The new file is invalid; the previous set stays.
    pub fn reload(&mut self, path: impl AsRef<Path>) -> Result<bool, String> {
        let key = key(path.as_ref());
        if !self.sets.contains_key(&key) {
            return Ok(false);
        }
        let set = Arc::new(read_scatter(&key)?);
        if let Some(entry) = self.sets.get_mut(&key) {
            entry.0 = set;
            entry.1 += 1;
        }
        Ok(true)
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    a == b || key(a) == key(b)
}

/// `Update`: re-reads changed layer stacks and scatter sets (dev builds).
pub fn reload_terrain_files(
    mut changes: MessageReader<regia_asset::SourceAssetChanged>,
    mut terrains: Query<(&TerrainLayersSource, &mut Terrain)>,
    mut scatter: ResMut<ScatterSets>,
    mut status: Option<ResMut<regia_asset::AssetWatchStatus>>,
    assets: Option<Res<regia_asset::AssetServer>>,
) {
    use regia_asset::{SourceKind, WatchEntry, WatchOutcome};
    for change in changes.read() {
        let started = std::time::Instant::now();
        let outcome = match change.kind {
            SourceKind::TerrainLayers => {
                let users: Vec<_> = terrains
                    .iter_mut()
                    .filter(|(source, _)| same_file(&source.0, &change.path))
                    .collect();
                if users.is_empty() {
                    continue;
                }
                let parsed = std::fs::read_to_string(&change.path)
                    .map_err(|e| e.to_string())
                    .and_then(|t| TerrainLayers::from_ron(&t).map_err(|e| e.to_string()));
                match parsed {
                    Ok(layers) => {
                        for (_, mut terrain) in users {
                            terrain.layers = layers.clone();
                        }
                        WatchOutcome::Reloaded
                    }
                    Err(err) => WatchOutcome::Error(err),
                }
            }
            SourceKind::Scatter => match scatter.reload(&change.path) {
                Ok(false) => continue,
                Ok(true) => WatchOutcome::Reloaded,
                Err(err) => WatchOutcome::Error(err),
            },
            _ => continue,
        };
        let shown = match assets.as_deref() {
            Some(server) => regia_asset::display_path(server.project_root(), &change.path),
            None => change.path.display().to_string(),
        };
        match &outcome {
            WatchOutcome::Reloaded => tracing::info!(target: "regia_terrain", "reloaded {shown}"),
            WatchOutcome::Error(err) => {
                tracing::error!(target: "regia_terrain", "{shown}: {err} (keeping the previous version)")
            }
        }
        if let Some(status) = status.as_deref_mut() {
            status.record(WatchEntry {
                path: shown,
                kind: change.kind,
                millis: started.elapsed().as_millis() as u64,
                outcome,
            });
        }
    }
}
