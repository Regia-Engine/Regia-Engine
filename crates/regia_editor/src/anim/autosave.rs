// SPDX-License-Identifier: MPL-2.0
//! Document autosave of the animation graph editor (`crate::doc_autosave`, ADR-9770). The bytes
//! are the editor's `GraphModel` as RON, not the validated `.animgraph.ron` a save writes: a save
//! drops unreachable nodes and refuses an invalid graph, and recovery must keep both.

use regia_ecs::prelude::*;

use super::AnimEditor;
use super::model::GraphModel;
use crate::doc_autosave::{DocKey, DocProvider, DocSnapshot};

/// The provider kind.
pub(crate) const KIND: &str = "anim_graph";

/// The registry entry.
pub(crate) const PROVIDER: DocProvider = DocProvider {
    kind: KIND,
    dirty,
    snapshot,
    restore,
};

fn unsaved(world: &World) -> Option<&AnimEditor> {
    world
        .get_resource::<AnimEditor>()
        .filter(|e| e.graph_dirty() && (e.graph_path.is_some() || e.saved_graph.is_some() || e.revision > 0))
}

fn dirty(world: &World) -> Vec<DocKey> {
    unsaved(world)
        .map(|e| vec![DocKey::new(KIND, e.graph_path.as_deref())])
        .unwrap_or_default()
}

fn snapshot(world: &World) -> Vec<DocSnapshot> {
    let Some(editor) = unsaved(world) else {
        return Vec::new();
    };
    match ron::ser::to_string_pretty(&editor.docs.graph, regia_core::ron_pretty()) {
        Ok(text) => vec![DocSnapshot {
            key: DocKey::new(KIND, editor.graph_path.as_deref()),
            bytes: text.into_bytes(),
        }],
        Err(err) => {
            tracing::warn!(target: "regia_editor", "anim graph autosave: {err}");
            Vec::new()
        }
    }
}

/// Opens the original graph when it exists (it is the saved copy), then puts the recovered model
/// over it: dirty until a save.
fn restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not text: {e}"))?;
    let model: GraphModel = ron::from_str(text).map_err(|e| format!("cannot read the autosave: {e}"))?;
    let mut editor = world.get_resource_or_insert_with(AnimEditor::default);
    match key.original.as_deref().filter(|path| path.is_file()) {
        Some(path) => editor.open_graph(path)?,
        None => editor.new_graph(key.original.clone()),
    }
    editor.docs.graph = model;
    editor.revision += 1;
    editor.toast = format!("recovered {}", key.label);
    Ok(())
}

/// The Save all hook (P-147, ADR-8147): the animation graph editor's Save of the graph file (an
/// invalid graph is refused, as its Save refuses it).
pub(crate) fn save(world: &mut World, _key: &DocKey) -> Result<(), String> {
    if unsaved(world).is_none() {
        return Ok(());
    }
    world.resource_mut::<AnimEditor>().save_graph(None).map(|_| ())
}
