// SPDX-License-Identifier: MPL-2.0
//! Document autosave of the UI animation editor (`crate::doc_autosave`, ADR-9770): the open
//! library as [`UiAnimDoc::text`] writes it (the `.uianim.ron` a save writes), restored dirty at
//! its original path.

use regia_ecs::prelude::*;

use crate::doc_autosave::{DocKey, DocProvider, DocSnapshot};
use crate::ui_anim_doc::{UiAnimDoc, UiAnimEditor};

/// The provider kind.
pub(crate) const KIND: &str = "ui_anim";

/// The registry entry.
pub(crate) const PROVIDER: DocProvider = DocProvider {
    kind: KIND,
    dirty,
    snapshot,
    restore,
};

fn dirty(world: &World) -> Vec<DocKey> {
    world
        .get_resource::<UiAnimEditor>()
        .and_then(UiAnimEditor::doc)
        .filter(|doc| doc.dirty())
        .map(|doc| vec![DocKey::new(KIND, doc.path.as_deref())])
        .unwrap_or_default()
}

fn snapshot(world: &World) -> Vec<DocSnapshot> {
    let Some(doc) = world
        .get_resource::<UiAnimEditor>()
        .and_then(UiAnimEditor::doc)
        .filter(|d| d.dirty())
    else {
        return Vec::new();
    };
    match doc.text() {
        Ok(text) => vec![DocSnapshot {
            key: DocKey::new(KIND, doc.path.as_deref()),
            bytes: text.into_bytes(),
        }],
        Err(err) => {
            tracing::warn!(target: "regia_editor", "UI animation autosave: {err}");
            Vec::new()
        }
    }
}

/// Opens the original file when it exists (it is the saved copy), then puts the recovered
/// library over it: the document is dirty at its original path until a save.
fn restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not text: {e}"))?;
    let lib = regia_ui::anim_timeline::UiAnimLibrary::from_ron(text)
        .map_err(|e| format!("cannot read the autosave: {e}"))?;
    let mut doc = match key.original.as_deref().filter(|path| path.is_file()) {
        Some(path) => UiAnimDoc::open(path).map_err(|e| e.to_string())?,
        None => UiAnimDoc::new_document(key.original.clone()),
    };
    doc.restore_library(lib);
    world
        .get_resource_or_insert_with(UiAnimEditor::default)
        .set_doc(doc);
    Ok(())
}

/// The Save all hook (P-147, ADR-8147): the UI animation editor's Save to the library's file.
pub(crate) fn save(world: &mut World, _key: &DocKey) -> Result<(), String> {
    if dirty(world).is_empty() {
        return Ok(());
    }
    world
        .resource_mut::<UiAnimEditor>()
        .doc_mut()
        .and_then(|doc| doc.save(None))
        .map(|_| ())
        .map_err(|e| e.to_string())
}
