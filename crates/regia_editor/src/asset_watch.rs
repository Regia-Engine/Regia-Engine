// SPDX-License-Identifier: MPL-2.0
//! Live asset reload in the editor (`docs/specs/12-assets.md` § 8, `docs/specs/19-editor.md`).
//!
//! Opening a project starts a [`LiveCooker`] on its `assets/` (plus the engine's WGSL directory in
//! a dev checkout). Each frame [`pump_asset_watch`] hands its results to the edit world and, while
//! play-in-editor runs, to the play world (the same [`ReloadMessage`]s, so cooked assets, shaders,
//! material graphs, VFX, UI layouts, audio and terrain files reload in both). The editor then
//! reacts to its own documents:
//!
//! - the open level changed on disk: reload it in place, keeping the editor camera; if the level
//!   has unsaved edits, ask first ([`PendingLevelReload`], the shell shows a prompt);
//! - a prefab changed: re-bake it and re-instantiate its live instances.
//!
//! The status line shows the latest [`AssetWatchStatus`] entry ("reloaded X in 120 ms" /
//! "cook error in Y: …"); agents read the same through `assets.watch_status`. Agents answer the
//! changed-on-disk prompt with `editor.level_reload_resolve { choice }` ([`methods`], ADR-7747).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::Receiver;
use std::time::Instant;

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset::{
    AssetWatchStatus, ReloadMessage, SourceKind, WatchEntry, WatchOutcome, apply_reload_messages,
    display_path,
};
use regia_asset_pipeline::{LiveCooker, LiveCookerConfig};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::command::UndoStack;
use crate::level_io::{self, OpenLevel};

/// Turns live asset reload on for projects opened in this world ([`crate::project::open_project`]
/// starts the watcher when present and enabled). The editor binary inserts it unless
/// `REGIA_ASSET_WATCH=0`; tests that open projects leave it out.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveAssetReload {
    /// Start a watcher when a project opens.
    pub enabled: bool,
}

impl LiveAssetReload {
    /// Enabled unless the `REGIA_ASSET_WATCH` environment variable is `0`.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            enabled: regia_core::brand::env_var("REGIA_ASSET_WATCH").map_or(true, |v| v != "0"),
        }
    }
}

/// The editor's live cooker and its result channel (edit world resource).
#[derive(Resource)]
pub struct EditorAssetWatch {
    _cooker: LiveCooker,
    rx: Mutex<Receiver<ReloadMessage>>,
    project_root: PathBuf,
}

/// The open level changed on disk while it had unsaved edits: waiting for the user's choice
/// ([`resolve_level_reload`]).
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct PendingLevelReload {
    /// The changed level file.
    pub path: PathBuf,
}

/// Starts watching `project_root` (replacing a previous watch). Status goes to
/// [`AssetWatchStatus`].
///
/// # Errors
///
/// The file watcher could not start; editing keeps working without live reload.
pub fn start_asset_watch(world: &mut World, project_root: &Path) -> Result<(), String> {
    world.remove_resource::<EditorAssetWatch>();
    let mut config = LiveCookerConfig::for_host(project_root.to_path_buf());
    if let Some(shaders) = regia_render::shader_reload::engine_shader_dir()
        && shaders.is_dir()
    {
        config.extra_roots.push(shaders);
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let cooker = LiveCooker::spawn(config, tx).map_err(|e| format!("asset watcher: {e}"))?;
    let mut status = world.get_resource_or_insert_with(AssetWatchStatus::default);
    status.watching = true;
    status.roots = cooker.roots().to_vec();
    status.debounce_ms = cooker.quiet().as_millis() as u64;
    tracing::info!(target: "regia_editor", roots = ?cooker.roots(), "live asset reload on");
    world.insert_resource(EditorAssetWatch {
        _cooker: cooker,
        rx: Mutex::new(rx),
        project_root: project_root.to_path_buf(),
    });
    Ok(())
}

/// Delivers the live cooker's results to the edit world (and `play`, when PIE runs) and applies
/// the editor's own reactions. Returns how many messages were delivered. Call once per frame.
pub fn pump_asset_watch(edit: &mut World, play: Option<&mut World>) -> usize {
    let messages: Vec<ReloadMessage> = match edit.get_resource::<EditorAssetWatch>() {
        Some(watch) => match watch.rx.lock() {
            Ok(rx) => rx.try_iter().collect(),
            Err(_) => Vec::new(),
        },
        None => return 0,
    };
    if messages.is_empty() {
        return 0;
    }
    let count = messages.len();
    let changed: Vec<PathBuf> = messages
        .iter()
        .filter_map(|m| match m {
            ReloadMessage::Source(path) => Some(path.clone()),
            _ => None,
        })
        .collect();
    let cooked: Vec<PathBuf> = messages
        .iter()
        .filter_map(|m| match m {
            ReloadMessage::Status(entry) => Some(PathBuf::from(&entry.path)),
            _ => None,
        })
        .collect();
    // A standalone game the editor launched gets the same changes over `assets.push` (ADR-7836).
    crate::launch::forward_reloads(edit, &messages);
    if let Some(play) = play {
        apply_reload_messages(play, messages.clone());
    }
    apply_reload_messages(edit, messages);
    // The Content Browser re-lists only the folders that hold the changed files, and their
    // thumbnails are made again when the file's time or length changed (ADR-1329). Cooked
    // sources only report through their status entry (`assets/...`).
    let mut browsed = changed.clone();
    browsed.extend(cooked);
    crate::content_index::note_changed(edit, &browsed);
    crate::thumb_cache::note_changed(edit, &browsed);
    for path in changed {
        match SourceKind::of(&path) {
            SourceKind::Level => {
                // A change in a level directory arrives as the entity file that changed.
                let root = regia_scene::level_dir::containing_level_dir(&path);
                on_level_changed(edit, root.as_deref().unwrap_or(&path));
            }
            SourceKind::Prefab => on_prefab_changed(edit, &path),
            _ => {}
        }
    }
    count
}

fn shown(world: &World, path: &Path) -> String {
    match world.get_resource::<EditorAssetWatch>() {
        Some(watch) => display_path(&watch.project_root, path),
        None => path.display().to_string(),
    }
}

fn record(world: &mut World, path: &Path, kind: SourceKind, started: Instant, outcome: WatchOutcome) {
    let entry = WatchEntry {
        path: shown(world, path),
        kind,
        millis: started.elapsed().as_millis() as u64,
        outcome,
    };
    let line = world
        .get_resource_or_insert_with(AssetWatchStatus::default)
        .record(entry);
    tracing::info!(target: "regia_editor", "{line}");
}

fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || match (a.canonicalize(), b.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
}

/// The open level's file changed: nothing to do when it matches the edit world (the editor's
/// own save), reload when there are no unsaved edits, otherwise ask. The watcher calls it for
/// every change event of a level file; it is public so a test can deliver one deterministically.
/// The edit world is compared with a running sequence preview lifted (the values the save
/// wrote), so a save made during a preview is still recognized as the editor's own.
pub fn on_level_changed(world: &mut World, path: &Path) {
    let Some(open) = world.get_resource::<OpenLevel>().cloned() else {
        return;
    };
    if !open.path.as_deref().is_some_and(|p| same_file(p, path)) {
        return;
    }
    let on_disk = regia_scene::level_dir::read_level_source(path)
        .ok()
        .and_then(|scene| {
            regia_scene::prelude::Level::from_scene_file(open.id, scene)
                .to_ron_string()
                .ok()
        });
    let in_editor = crate::seq_tracks::with_preview_lifted(world, |world| {
        crate::level_io::snapshot_saved_level(world, open.id)
    })
    .ok()
    .and_then(|level| {
        let mut scene = level.scene;
        scene.entities.sort_by_key(|e| e.guid);
        regia_scene::prelude::Level::from_scene_file(open.id, scene)
            .to_ron_string()
            .ok()
    });
    if on_disk.is_some() && on_disk == in_editor {
        return;
    }
    let dirty = world.get_resource::<UndoStack>().is_some_and(UndoStack::is_dirty);
    if dirty {
        tracing::info!(target: "regia_editor", "{} changed on disk; asking before discarding unsaved edits", path.display());
        world.insert_resource(PendingLevelReload {
            path: path.to_path_buf(),
        });
        return;
    }
    reload_level_keep_camera(world, path);
}

/// The user's answer to [`PendingLevelReload`]: `reload = true` discards the unsaved edits and
/// reloads from disk; `false` keeps the edits (the next save overwrites the file).
pub fn resolve_level_reload(world: &mut World, reload: bool) {
    let Some(pending) = world.remove_resource::<PendingLevelReload>() else {
        return;
    };
    if reload {
        reload_level_keep_camera(world, &pending.path);
    }
}

/// Re-opens `path` as the current level. The editor camera and view settings are resources, not
/// level content, so they stay; the selection and the undo history (whose entity ids die with the
/// old level) are cleared.
pub fn reload_level_keep_camera(world: &mut World, path: &Path) {
    let started = Instant::now();
    match level_io::open_level_file(world, path) {
        Ok(open) => {
            world.insert_resource(open);
            world.insert_resource(UndoStack::new());
            if let Some(mut selection) = world.get_resource_mut::<crate::selection::Selection>() {
                selection.clear();
            }
            record(world, path, SourceKind::Level, started, WatchOutcome::Reloaded);
        }
        Err(err) => record(world, path, SourceKind::Level, started, WatchOutcome::Error(err)),
    }
}

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

/// `editor.level_reload_resolve { choice? }`: the status bar's "Reload from disk" / "Keep mine"
/// prompt. `choice: "reload"` discards the unsaved edits and reloads the level from disk,
/// `"keep_mine"` keeps them (the next save overwrites the file); without `choice` it only reports
/// whether a prompt is pending.
fn level_reload_resolve(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    const HINT: &str = "editor.level_reload_resolve takes an optional choice: \"reload\" or \"keep_mine\"";
    let pending = world.get_resource::<PendingLevelReload>().cloned();
    let reload = match params.get("choice") {
        None | Some(Value::Null) => {
            return Ok(match &pending {
                Some(pending) => json!({ "pending": true, "path": shown(world, &pending.path) }),
                None => json!({ "pending": false }),
            });
        }
        Some(Value::String(choice)) if choice == "reload" => true,
        Some(Value::String(choice)) if choice == "keep_mine" => false,
        Some(other) => {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "bad_choice",
                format!("choice must be \"reload\" or \"keep_mine\", got {other}"),
                HINT,
            ));
        }
    };
    let Some(pending) = pending else {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "no_pending_reload",
            "the open level has no changed-on-disk prompt waiting".to_string(),
            "the prompt appears when the open level's file changes on disk while it has unsaved edits; \
             call without choice to check",
        ));
    };
    let path = shown(world, &pending.path);
    resolve_level_reload(world, reload);
    // The reload's own status line ("reloaded X in N ms" or the open error); none for keep_mine.
    let last = reload
        .then(|| {
            world
                .get_resource::<AssetWatchStatus>()
                .and_then(|status| status.recent.back().map(WatchEntry::toast))
        })
        .flatten();
    Ok(json!({
        "choice": if reload { "reload" } else { "keep_mine" },
        "path": path,
        "reloaded": reload,
        "status": last,
    }))
}

/// The `editor.level_reload_resolve` method (ADR-7747).
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.level_reload_resolve",
            tool: "editor_level_reload_resolve",
            description: "Answer the status bar's prompt that the open level changed on disk while it \
                has unsaved edits (the \"Reload from disk\" and \"Keep mine\" buttons): \
                choice \"reload\" discards the unsaved edits and reloads the level (the undo history \
                and selection are cleared, the editor camera stays); \"keep_mine\" keeps the edits \
                and the next save overwrites the file. Without choice it only reports whether a \
                prompt is pending. Errors: bad_choice, no_pending_reload.",
            params: &[ParamSchema {
                name: "choice",
                kind: "string",
                required: false,
                description: "\"reload\" or \"keep_mine\"; absent = report { pending, path? } only.",
            }],
            result: "{ pending, path? } without choice; { choice, path, reloaded, status } with one",
            mutating: true,
            permission: Permission::EditScene,
        },
        level_reload_resolve as Handler,
    )]
}

fn on_prefab_changed(world: &mut World, path: &Path) {
    let started = Instant::now();
    let outcome = match crate::prefab_edit::reload_prefab_file(world, path) {
        Ok(0) => return,
        Ok(_) => WatchOutcome::Reloaded,
        Err(err) => WatchOutcome::Error(err),
    };
    record(world, path, SourceKind::Prefab, started, outcome);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_file_compares_through_symlinks_and_text() {
        let dir = std::env::temp_dir();
        assert!(same_file(&dir, &dir));
        assert!(
            same_file(&dir, &dir.join(".")),
            "different spellings of one directory are the same file"
        );
        assert!(!same_file(
            Path::new("/nonexistent/a"),
            Path::new("/nonexistent/b")
        ));
    }
}
