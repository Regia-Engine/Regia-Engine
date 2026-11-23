// SPDX-License-Identifier: MPL-2.0
//! Document autosave of the behavior tree (logic graph) editor (`crate::doc_autosave`,
//! ADR-9770): the open tree as its canonical save text.

use regia_ai::bt_edit;
use regia_ecs::prelude::*;

use super::BtEditor;
use crate::doc_autosave::{DocKey, DocProvider, DocSnapshot};

/// The provider kind.
pub(crate) const KIND: &str = "behavior_tree";

/// The registry entry.
pub(crate) const PROVIDER: DocProvider = DocProvider {
    kind: KIND,
    dirty,
    snapshot,
    restore,
};

fn unsaved(world: &World) -> Option<&BtEditor> {
    world
        .get_resource::<BtEditor>()
        .filter(|e| e.is_open() && e.dirty())
}

fn dirty(world: &World) -> Vec<DocKey> {
    unsaved(world)
        .map(|e| vec![DocKey::new(KIND, e.path.as_deref())])
        .unwrap_or_default()
}

fn snapshot(world: &World) -> Vec<DocSnapshot> {
    let Some(editor) = unsaved(world) else {
        return Vec::new();
    };
    match bt_edit::to_canonical_ron(&editor.doc) {
        Ok(text) => vec![DocSnapshot {
            key: DocKey::new(KIND, editor.path.as_deref()),
            bytes: text.into_bytes(),
        }],
        Err(err) => {
            tracing::warn!(target: "regia_editor", "behavior tree autosave: {err}");
            Vec::new()
        }
    }
}

/// Opens the recovered tree at its original path, never saved in this session (dirty).
fn restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not text: {e}"))?;
    let doc = bt_edit::parse_bt_ron(text)?;
    let mut editor = world.get_resource_or_insert_with(BtEditor::default);
    editor.doc = doc;
    editor.saved = None;
    editor.path = key.original.clone();
    editor.session += 1;
    editor.revision += 1;
    editor.selected = None;
    editor.toast = format!("recovered {}", key.label);
    Ok(())
}

/// The Save all hook (P-147, ADR-8147): the logic graph editor's Save to the tree's file.
pub(crate) fn save(world: &mut World, _key: &DocKey) -> Result<(), String> {
    if unsaved(world).is_none() {
        return Ok(());
    }
    let valid = bt_edit::is_valid(&super::diagnose(world));
    world.resource_mut::<BtEditor>().save(None, valid).map(|_| ())
}
