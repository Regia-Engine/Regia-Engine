// SPDX-License-Identifier: MPL-2.0
//! Document autosave of prefab edit mode (`crate::doc_autosave`, ADR-9770, ADR-7382): the open
//! prefab, while it has unsaved edits ([`crate::prefab_edit::is_dirty`]), as the text
//! `save_prefab` would write, keyed by the `.prefab.ron` path.
//!
//! The prefab's contents are a whole-scene snapshot, which needs the world mutably, so the
//! provider's preparation hook ([`prepare`], run by the tick just before the snapshots) takes the
//! text and the snapshot hook hands it over. The one-root rule is a save's only: an edit that has
//! two roots for a moment is still autosaved. **Restore** re-enters prefab edit on the original
//! path with the recovered contents, dirty until a save (`editor.close_prefab {save: true}` or
//! `editor.save_prefab`); closing without a save drops the edit and its autosave.

use regia_ecs::prelude::*;

use crate::doc_autosave::{DocKey, DocProvider, DocSnapshot};
use crate::prefab_edit::{PrefabEditMode, enter_prefab_edit_with_text, in_prefab_mode, is_dirty};

/// The provider kind.
pub(crate) const KIND: &str = "prefab";

/// The registry entry.
pub(crate) const PROVIDER: DocProvider = DocProvider {
    kind: KIND,
    dirty,
    snapshot,
    restore,
};

/// Takes the dirty prefab's text (the tick's preparation, before [`snapshot`]).
pub(crate) fn prepare(world: &mut World) {
    crate::prefab_edit::prepare_autosave(world);
}

fn key(world: &World) -> Option<DocKey> {
    let mode = world.get_resource::<PrefabEditMode>()?;
    mode.editing().map(|path| DocKey::new(KIND, Some(path)))
}

fn dirty(world: &World) -> Vec<DocKey> {
    if !is_dirty(world) {
        return Vec::new();
    }
    key(world).into_iter().collect()
}

fn snapshot(world: &World) -> Vec<DocSnapshot> {
    if !is_dirty(world) {
        return Vec::new();
    }
    let Some(text) = world
        .get_resource::<PrefabEditMode>()
        .and_then(PrefabEditMode::autosave_text)
    else {
        return Vec::new();
    };
    key(world)
        .map(|key| DocSnapshot {
            key,
            bytes: text.as_bytes().to_vec(),
        })
        .into_iter()
        .collect()
}

/// Re-enters prefab edit on the original path with the recovered contents (the level that is
/// open is stashed, as for any prefab).
fn restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not text: {e}"))?;
    let path = key
        .original
        .as_deref()
        .ok_or_else(|| "a prefab autosave needs its file's path".to_string())?;
    if in_prefab_mode(world) {
        return Err("a prefab is open: close it, then restore this one".to_string());
    }
    enter_prefab_edit_with_text(world, path, text)
}

/// The Save all hook (P-147, ADR-8147): prefab edit mode's Save.
pub(crate) fn save(world: &mut World, _key: &DocKey) -> Result<(), String> {
    if !(in_prefab_mode(world) && is_dirty(world)) {
        return Ok(());
    }
    crate::prefab_edit::save_prefab(world).map(|_| ())
}
