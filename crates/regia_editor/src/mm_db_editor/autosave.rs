// SPDX-License-Identifier: MPL-2.0
//! Document autosave of the motion-database editor (`crate::doc_autosave`, ADR-9770): the open
//! `.motiondb.ron` as its canonical save text (`MotionDatabaseSource::to_ron_string`).

use regia_animation::motion_matching::MotionDatabaseSource;
use regia_ecs::prelude::*;

use super::MmDbEditor;
use crate::doc_autosave::{DocKey, DocProvider, DocSnapshot};

/// The provider kind.
pub(crate) const KIND: &str = "motion_db";

/// The registry entry.
pub(crate) const PROVIDER: DocProvider = DocProvider {
    kind: KIND,
    dirty,
    snapshot,
    restore,
};

fn unsaved(world: &World) -> Option<&MmDbEditor> {
    world.get_resource::<MmDbEditor>().filter(|e| e.is_dirty())
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
    let Some(doc) = editor.doc.as_ref() else {
        return Vec::new();
    };
    match super::canonical_text(doc) {
        Ok(text) => vec![DocSnapshot {
            key: DocKey::new(KIND, editor.path.as_deref()),
            bytes: text.into_bytes(),
        }],
        Err(err) => {
            tracing::warn!(target: "regia_editor", "motion database autosave: {}", err.message);
            Vec::new()
        }
    }
}

/// Opens the recovered database at its original path, never saved in this session (dirty), and
/// shows the window.
fn restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not text: {e}"))?;
    let doc = MotionDatabaseSource::from_ron_str(text).map_err(|e| e.to_string())?;
    let mut editor = world.get_resource_or_insert_with(MmDbEditor::default);
    editor.recover(key.original.clone(), doc);
    editor.toast = format!("recovered {}", key.label);
    Ok(())
}

/// The Save all hook (P-147, ADR-8147): the open motion database the key names.
///
/// An untitled database is saved from the editor. A key that does not name the open
/// database is a no-op so a stale row does not fail the rest of Save all.
pub(crate) fn save(world: &mut World, key: &DocKey) -> Result<(), String> {
    let Some(original) = key.original.as_deref() else {
        return Err("untitled: save it from the motion database editor".to_string());
    };
    let matches = world
        .get_resource::<super::MmDbEditor>()
        .is_some_and(|editor| editor.path.as_deref() == Some(original) && editor.is_dirty());
    if !matches {
        return Ok(());
    }
    super::save(world, None).map(|_| ()).map_err(|err| err.message)
}
