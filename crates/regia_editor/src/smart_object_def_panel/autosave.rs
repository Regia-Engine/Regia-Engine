// SPDX-License-Identifier: MPL-2.0
//! Document autosave of the smart-object definition editor (`crate::doc_autosave`, ADR-9770): the
//! open definition as `SmartObjectDef::to_ron`, which its save writes.

use std::path::{Path, PathBuf};

use regia_ai::smart_object_def::SmartObjectDef;
use regia_ecs::prelude::*;

use super::SmartObjectDefPanel;
use crate::doc_autosave::{DocKey, DocProvider, DocSnapshot};

/// The provider kind.
pub(crate) const KIND: &str = "smart_object";

/// The registry entry.
pub(crate) const PROVIDER: DocProvider = DocProvider {
    kind: KIND,
    dirty,
    snapshot,
    restore,
};

fn dirty(world: &World) -> Vec<DocKey> {
    world
        .get_resource::<SmartObjectDefPanel>()
        .and_then(SmartObjectDefPanel::doc)
        .filter(|doc| doc.dirty())
        .map(|doc| vec![DocKey::new(KIND, Some(Path::new(&doc.path)))])
        .unwrap_or_default()
}

fn snapshot(world: &World) -> Vec<DocSnapshot> {
    let Some(doc) = world
        .get_resource::<SmartObjectDefPanel>()
        .and_then(SmartObjectDefPanel::doc)
        .filter(|doc| doc.dirty())
    else {
        return Vec::new();
    };
    match doc.def.to_ron() {
        Ok(text) => vec![DocSnapshot {
            key: DocKey::new(KIND, Some(Path::new(&doc.path))),
            bytes: text.into_bytes(),
        }],
        Err(err) => {
            tracing::warn!(target: "regia_editor", "smart object definition autosave: {err}");
            Vec::new()
        }
    }
}

/// Opens the original definition, then puts the recovered one over it (dirty). A recovered
/// definition the validator refuses is still restored (it is what the user had); Save reports it.
fn restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let original = key
        .original
        .as_deref()
        .ok_or_else(|| "a smart object definition autosave needs its file".to_string())?;
    let root: Option<PathBuf> = crate::matgraph_editor::project_root(world);
    let rel = root
        .as_deref()
        .and_then(|root| original.strip_prefix(root).ok())
        .unwrap_or(original)
        .to_string_lossy()
        .replace('\\', "/");
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not text: {e}"))?;
    let def: SmartObjectDef = match SmartObjectDef::from_ron(text) {
        Ok(def) => def,
        Err(regia_ai::smart_object_def::SmartObjectDefError::BadDefinition(_)) => {
            ron::from_str(text).map_err(|e| e.to_string())?
        }
        Err(e) => return Err(e.to_string()),
    };
    super::open_def(world, &rel).map_err(|e| e.message)?;
    let mut panel = world.get_resource_or_insert_with(SmartObjectDefPanel::default);
    if let Some(doc) = panel.doc_mut() {
        doc.def = def;
    }
    Ok(())
}

/// The Save all hook (P-147, ADR-8147): the smart object definition editor's Save (refused while
/// the runtime validator fails, as its Save).
pub(crate) fn save(world: &mut World, _key: &DocKey) -> Result<(), String> {
    super::save_def(world)
        .map(|_| ())
        .map_err(|e| format!("{} ({})", e.message, e.code))
}
