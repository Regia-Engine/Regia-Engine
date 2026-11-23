// SPDX-License-Identifier: MPL-2.0
//! Document autosave of the UI designer (`crate::doc_autosave`, ADR-9770): the open document,
//! as [`super::UiDesign::save_text`] would write it.

use regia_ecs::prelude::*;

use super::UiDesigner;
use crate::doc_autosave::{DocKey, DocProvider, DocSnapshot};

/// The provider kind.
pub(crate) const KIND: &str = "ui_design";

/// The registry entry.
pub(crate) const PROVIDER: DocProvider = DocProvider {
    kind: KIND,
    dirty,
    snapshot,
    restore,
};

fn dirty(world: &World) -> Vec<DocKey> {
    world
        .get_resource::<UiDesigner>()
        .and_then(UiDesigner::doc)
        .filter(|doc| doc.dirty())
        .map(|doc| vec![DocKey::new(KIND, doc.path.as_deref())])
        .unwrap_or_default()
}

fn snapshot(world: &World) -> Vec<DocSnapshot> {
    let Some(doc) = world
        .get_resource::<UiDesigner>()
        .and_then(UiDesigner::doc)
        .filter(|d| d.dirty())
    else {
        return Vec::new();
    };
    match doc.save_text() {
        Ok((text, _)) => vec![DocSnapshot {
            key: DocKey::new(KIND, doc.path.as_deref()),
            bytes: text.into_bytes(),
        }],
        Err(err) => {
            tracing::warn!(target: "regia_editor", "UI designer autosave: {err}");
            Vec::new()
        }
    }
}

/// Opens the text at the original path and marks it changed (dirty until a save).
fn restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not text: {e}"))?;
    let mut designer = world.get_resource_or_insert_with(UiDesigner::default);
    designer
        .open_text(text, key.original.clone())
        .map_err(|e| e.to_string())?;
    if let Some(doc) = designer.doc.as_mut() {
        // Never saved in this session: dirty until the user saves.
        doc.saved_revision = doc.revision.wrapping_sub(1);
    }
    Ok(())
}

/// The Save all hook (P-147, ADR-8147): the UI designer's Save to the document's file.
pub(crate) fn save(world: &mut World, _key: &DocKey) -> Result<(), String> {
    let Some(mut designer) = world.get_resource_mut::<UiDesigner>() else {
        return Ok(());
    };
    if !designer.doc().is_some_and(|doc| doc.dirty()) {
        return Ok(());
    }
    designer
        .doc_mut()
        .and_then(|doc| doc.save(None))
        .map(|_| ())
        .map_err(|e| e.to_string())
}
