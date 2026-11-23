// SPDX-License-Identifier: MPL-2.0
//! Document autosave of the state tree editor (`crate::doc_autosave`, ADR-9770): the open tree
//! as its canonical save text.

use regia_ai::state_tree_edit;
use regia_ecs::prelude::*;

use super::StEditor;
use crate::doc_autosave::{DocKey, DocProvider, DocSnapshot};

/// The provider kind.
pub(crate) const KIND: &str = "state_tree";

/// The registry entry.
pub(crate) const PROVIDER: DocProvider = DocProvider {
    kind: KIND,
    dirty,
    snapshot,
    restore,
};

fn unsaved(world: &World) -> Option<&StEditor> {
    world
        .get_resource::<StEditor>()
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
    match state_tree_edit::to_canonical_ron(&editor.doc) {
        Ok(text) => vec![DocSnapshot {
            key: DocKey::new(KIND, editor.path.as_deref()),
            bytes: text.into_bytes(),
        }],
        Err(err) => {
            tracing::warn!(target: "regia_editor", "state tree autosave: {}", err.message);
            Vec::new()
        }
    }
}

/// Opens the recovered tree at its original path, never saved in this session (dirty).
fn restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not text: {e}"))?;
    let doc = state_tree_edit::parse_statetree_ron(text)?;
    let mut editor = world.get_resource_or_insert_with(StEditor::default);
    editor.doc = doc;
    editor.saved = None;
    editor.path = key.original.clone();
    editor.session += 1;
    editor.revision += 1;
    editor.selected = None;
    editor.reload = None;
    editor.toast = format!("recovered {}", key.label);
    Ok(())
}

/// The Save all hook (P-147, ADR-8147): the state tree editor's Save to the tree's file (a tree
/// with validation errors is refused, as its Save refuses it).
pub(crate) fn save(world: &mut World, _key: &DocKey) -> Result<(), String> {
    if unsaved(world).is_none() {
        return Ok(());
    }
    super::save(world, None)
        .map(|_| ())
        .map_err(|e| format!("{} ({})", e.message, e.code))
}
