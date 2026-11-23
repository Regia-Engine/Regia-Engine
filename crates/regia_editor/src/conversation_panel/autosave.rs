// SPDX-License-Identifier: MPL-2.0
//! Document autosave of the conversation editor (`crate::doc_autosave`, ADR-9770): the open
//! file's text and its layout sidecar, as a RON envelope.

use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use super::layout::LayoutSidecar;
use super::{ConvEditor, Snapshot};
use crate::doc_autosave::{DocKey, DocProvider, DocSnapshot};

/// The provider kind.
pub(crate) const KIND: &str = "conversation";

/// The registry entry.
pub(crate) const PROVIDER: DocProvider = DocProvider {
    kind: KIND,
    dirty,
    snapshot,
    restore,
};

#[derive(Serialize, Deserialize)]
struct Envelope {
    text: String,
    sidecar: LayoutSidecar,
}

fn unsaved(world: &World) -> Option<&ConvEditor> {
    world
        .get_resource::<ConvEditor>()
        .filter(|e| e.path.is_some() && e.dirty())
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
    let envelope = Envelope {
        text: editor.doc.text.clone(),
        sidecar: editor.doc.sidecar.clone(),
    };
    match ron::ser::to_string_pretty(&envelope, regia_core::ron_pretty()) {
        Ok(text) => vec![DocSnapshot {
            key: DocKey::new(KIND, editor.path.as_deref()),
            bytes: text.into_bytes(),
        }],
        Err(err) => {
            tracing::warn!(target: "regia_editor", "conversation autosave: {err}");
            Vec::new()
        }
    }
}

/// Opens the original file, then puts the recovered text and positions over it (dirty).
fn restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let original = key
        .original
        .clone()
        .ok_or_else(|| "a conversation autosave needs its file".to_string())?;
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not text: {e}"))?;
    let envelope: Envelope = ron::from_str(text).map_err(|e| format!("cannot read the autosave: {e}"))?;
    let root = crate::matgraph_editor::project_root(world);
    let mut editor = world.get_resource_or_insert_with(ConvEditor::default);
    editor.open(&original, root.as_deref())?;
    editor.doc = Snapshot {
        text: envelope.text,
        sidecar: envelope.sidecar,
    };
    editor.revision += 1;
    editor.refresh();
    Ok(())
}

/// The Save all hook (P-147, ADR-8147): the conversation editor's Save (the file and its layout
/// sidecar).
pub(crate) fn save(world: &mut World, _key: &DocKey) -> Result<(), String> {
    if unsaved(world).is_none() {
        return Ok(());
    }
    world.resource_mut::<ConvEditor>().save(None).map(|_| ())
}
