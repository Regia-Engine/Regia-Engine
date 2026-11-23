// SPDX-License-Identifier: MPL-2.0
//! The undo history of each open camera asset (ADR-7734) and the `camera` document autosave
//! provider (`crate::doc_autosave`, ADR-9770).
//!
//! Each asset on the Cameras panel has a [`CameraDoc`]: one [`UndoStack`] (the editor's shared
//! history type), the working copy it last left, and whether the file had comments. Every field edit
//! and structure edit is one [`AssetEdit`] on that stack, which swaps the whole working copy: an
//! asset is small, and a whole-value swap cannot drift from the edit that made it. A continuous GUI
//! edit (a drag, typing in a field) merges into one step while the stack is not sealed; the panel
//! seals it when the pointer is released, and an agent edit is always its own step.
//!
//! A working copy changed outside the panel (the file reloaded from disk) no longer matches the
//! doc's last copy: the history then starts again from it, clean.

use std::any::Any;
use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use regia_framework::camera::asset::CameraAsset;
use regia_framework::camera::driver::CameraAssets;

use crate::command::{EditorCommand, UndoStack};
use crate::doc_autosave::{DocKey, DocProvider, DocSnapshot};

/// What the panel and the agent reply show before the first edit of a file that has comments.
pub const COMMENTS_WARNING: &str = "This file has comments: an edit here saves it in its canonical form and drops \
    them (edit the file in a text editor to keep them).";

/// One open camera asset's history.
pub struct CameraDoc {
    /// The undo history.
    pub undo: UndoStack,
    /// The working copy as the last edit, undo or save left it.
    pub last: CameraAsset,
    /// The file on disk had `//` or `/* */` comments when the doc opened (a save drops them).
    pub comments: bool,
}

impl std::fmt::Debug for CameraDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CameraDoc")
            .field("undo_depth", &self.undo.undo_depth())
            .field("redo_depth", &self.undo.redo_depth())
            .field("dirty", &self.undo.is_dirty())
            .field("comments", &self.comments)
            .finish_non_exhaustive()
    }
}

impl CameraDoc {
    /// A clean doc for `working`.
    #[must_use]
    pub fn new(working: CameraAsset, comments: bool) -> Self {
        Self {
            undo: UndoStack::new(),
            last: working,
            comments,
        }
    }

    /// The history as the agent reads it.
    #[must_use]
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "dirty": self.undo.is_dirty(),
            "undo_depth": self.undo.undo_depth(),
            "redo_depth": self.undo.redo_depth(),
            "next_undo": self.undo.next_undo(),
            "next_redo": self.undo.next_redo(),
            "comments_warning": self.comments.then_some(COMMENTS_WARNING),
        })
    }
}

/// One undo step: the working copy of `asset` before and after an edit.
pub struct AssetEdit {
    /// The asset's name.
    pub asset: String,
    /// What the step did, for the Undo button.
    pub label: String,
    /// The working copy before.
    pub before: CameraAsset,
    /// The working copy after.
    pub after: CameraAsset,
    /// Steps with the same key merge while the stack is not sealed (a drag of one field);
    /// `None` never merges.
    pub merge_key: Option<String>,
}

fn write_working(world: &mut World, asset: &str, value: &CameraAsset) {
    if let Some(mut assets) = world.get_resource_mut::<CameraAssets>() {
        assets.edit(asset, |working| *working = value.clone());
    }
}

impl EditorCommand for AssetEdit {
    fn apply(&mut self, world: &mut World) {
        write_working(world, &self.asset, &self.after);
    }

    fn revert(&mut self, world: &mut World) {
        write_working(world, &self.asset, &self.before);
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let next: &dyn Any = next;
        let Some(next) = next.downcast_ref::<AssetEdit>() else {
            return false;
        };
        let same = self.merge_key.is_some() && self.merge_key == next.merge_key && self.asset == next.asset;
        if same {
            self.after = next.after.clone();
        }
        same
    }

    fn description(&self) -> String {
        format!("{}: {}", self.asset, self.label)
    }
}

/// The autosave provider kind.
pub(crate) const KIND: &str = "camera";

/// The registry entry (`crate::doc_autosave_providers`).
pub(crate) const PROVIDER: DocProvider = DocProvider {
    kind: KIND,
    dirty,
    snapshot,
    restore,
};

/// The key of an asset's autosave: its file (`assets/<path>`), or the asset's name while it has none.
fn key(name: &str, path: Option<&str>) -> DocKey {
    match path {
        Some(path) => DocKey::new(KIND, Some(&Path::new("assets").join(path))),
        None => DocKey {
            kind: KIND.to_owned(),
            original: None,
            label: format!("{name}.camera.ron (no file)"),
        },
    }
}

/// The assets with unsaved edits: name, path, working copy.
fn unsaved(world: &World) -> Vec<(String, Option<String>, CameraAsset)> {
    let (Some(panel), Some(assets)) = (
        world.get_resource::<super::CameraPanel>(),
        world.get_resource::<CameraAssets>(),
    ) else {
        return Vec::new();
    };
    panel
        .docs
        .iter()
        .filter(|(_, doc)| doc.undo.is_dirty())
        .filter_map(|(name, doc)| {
            let working = assets.working(name)?;
            // A copy reloaded from disk since is not this history's any more.
            (*working == doc.last).then(|| {
                (
                    name.clone(),
                    assets.path(name).map(str::to_owned),
                    working.clone(),
                )
            })
        })
        .collect()
}

fn dirty(world: &World) -> Vec<DocKey> {
    unsaved(world)
        .into_iter()
        .map(|(name, path, _)| key(&name, path.as_deref()))
        .collect()
}

fn snapshot(world: &World) -> Vec<DocSnapshot> {
    unsaved(world)
        .into_iter()
        .filter_map(|(name, path, working)| match working.to_ron() {
            Ok(text) => Some(DocSnapshot {
                key: key(&name, path.as_deref()),
                bytes: text.into_bytes(),
            }),
            Err(err) => {
                tracing::warn!(target: "regia_editor", "camera autosave of {name}: {err}");
                None
            }
        })
        .collect()
}

/// The camera path (relative to `assets/`) of an autosave's original file, which may be absolute
/// (a recovery resolves it against the project root) or project-relative.
fn camera_path(world: &World, original: &Path) -> Option<String> {
    let root: Option<PathBuf> = crate::mesh_source::project_root(world);
    let rel = root
        .as_deref()
        .and_then(|root| original.strip_prefix(root).ok())
        .unwrap_or(original);
    let rel = rel.strip_prefix("assets").unwrap_or(rel);
    let text = rel.to_string_lossy().replace('\\', "/");
    regia_framework::camera::load::normalize_camera_path(&text).ok()
}

/// Puts the recovered working copy on the page at its file, unsaved (dirty) until a save.
fn restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not text: {e}"))?;
    let asset = CameraAsset::from_ron(text).map_err(|e| e.to_string())?;
    let path = key
        .original
        .as_deref()
        .and_then(|original| camera_path(world, original));
    super::ensure_registry(world);
    let name = asset.name.clone();
    world.resource_scope(|world, mut assets: Mut<CameraAssets>| {
        let hooks = world.resource::<regia_framework::camera::director::CameraHooks>();
        assets.insert(asset.clone(), path, hooks);
    });
    let mut panel = world.get_resource_or_insert_with(super::CameraPanel::default);
    let mut doc = CameraDoc::new(asset, false);
    doc.undo.mark_dirty();
    panel.docs.insert(name.clone(), doc);
    panel.selected_asset = Some(name);
    panel.selected_node = None;
    Ok(())
}

/// The Save all hook (P-147, ADR-8147): the camera panel's Save of the asset the key names (an
/// asset with no file is saved from the panel).
pub(crate) fn save(world: &mut World, doc: &DocKey) -> Result<(), String> {
    let Some(name) = unsaved(world)
        .into_iter()
        .find(|(name, path, _)| key(name, path.as_deref()).id() == doc.id())
        .map(|(name, _, _)| name)
    else {
        return Ok(());
    };
    super::with_panel(world, |panel, world| super::save_from_panel(panel, world, &name))
        .map(|_| ())
        .map_err(|e| e.message)
}
