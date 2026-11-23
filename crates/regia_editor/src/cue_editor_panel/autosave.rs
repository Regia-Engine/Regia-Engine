// SPDX-License-Identifier: MPL-2.0
//! Document autosave of the sound cue editor (`crate::doc_autosave`, ADR-9770): the open cue as
//! `SoundCueDef::to_ron`, which its save writes.

use std::path::{Path, PathBuf};

use regia_audio::SoundCueDef;
use regia_ecs::prelude::*;

use super::CueEditorPanel;
use crate::doc_autosave::{DocKey, DocProvider, DocSnapshot};

/// The provider kind.
pub(crate) const KIND: &str = "sound_cue";

/// The registry entry.
pub(crate) const PROVIDER: DocProvider = DocProvider {
    kind: KIND,
    dirty,
    snapshot,
    restore,
};

fn dirty(world: &World) -> Vec<DocKey> {
    world
        .get_resource::<CueEditorPanel>()
        .and_then(CueEditorPanel::doc)
        .filter(|doc| doc.dirty())
        .map(|doc| vec![DocKey::new(KIND, Some(Path::new(&doc.path)))])
        .unwrap_or_default()
}

fn snapshot(world: &World) -> Vec<DocSnapshot> {
    let Some(doc) = world
        .get_resource::<CueEditorPanel>()
        .and_then(CueEditorPanel::doc)
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
            tracing::warn!(target: "regia_editor", "sound cue autosave: {err}");
            Vec::new()
        }
    }
}

/// Opens the original cue, then puts the recovered definition over it (dirty).
fn restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let original = key
        .original
        .as_deref()
        .ok_or_else(|| "a sound cue autosave needs its file".to_string())?;
    let root: Option<PathBuf> = crate::matgraph_editor::project_root(world);
    let rel = root
        .as_deref()
        .and_then(|root| original.strip_prefix(root).ok())
        .unwrap_or(original)
        .to_string_lossy()
        .replace('\\', "/");
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not text: {e}"))?;
    let def = SoundCueDef::from_ron(text).map_err(|e| e.to_string())?;
    super::open_cue(world, &rel).map_err(|e| e.message)?;
    let mut panel = world.get_resource_or_insert_with(CueEditorPanel::default);
    if let Some(doc) = panel.doc.as_mut() {
        doc.def = def;
    }
    Ok(())
}

/// The Save all hook (P-147, ADR-8147): the sound cue editor's Save.
pub(crate) fn save(world: &mut World, _key: &DocKey) -> Result<(), String> {
    super::save_cue(world)
        .map(|_| ())
        .map_err(|e| format!("{} ({})", e.message, e.code))
}
