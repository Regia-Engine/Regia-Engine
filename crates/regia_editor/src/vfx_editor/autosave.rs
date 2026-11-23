// SPDX-License-Identifier: MPL-2.0
//! Document autosave of the VFX editor (`crate::doc_autosave`, ADR-9770, ADR-7340): the open
//! effect as `VfxDoc::text_to_save` plus the disabled-module stash.
//!
//! `.vfx.ron` has no "enabled" field (frozen at v7): disabling a module takes its values out of
//! the effect into the document's stash, so the bytes a save writes hold neither. The autosave
//! entry is editor state (under `.regia/autosave/docs/`, never cooked or packaged), so it keeps
//! the stash in an envelope beside the effect text and recovery restores the A/B toggle with its
//! values. An entry written before the stash existed (bare effect text) still restores, without
//! the stash.

use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use regia_vfx::format::VfxSystemDef;
use serde::{Deserialize, Serialize};

use super::{Disabled, VfxEditorPanel};
use crate::doc_autosave::{DocKey, DocProvider, DocSnapshot};

/// The provider kind.
pub(crate) const KIND: &str = "vfx";

/// The registry entry.
pub(crate) const PROVIDER: DocProvider = DocProvider {
    kind: KIND,
    dirty,
    snapshot,
    restore,
};

/// The envelope's version (editor state, not a content format).
const ENVELOPE_VERSION: u32 = 1;

/// The editor-only autosave bytes: the effect text a save writes and the disabled-module stash.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Envelope {
    /// [`ENVELOPE_VERSION`].
    version: u32,
    /// The `.vfx.ron` text, as `VfxDoc::text_to_save` returns it.
    effect: String,
    /// Disabled modules (by emitter name), kept out of the effect.
    disabled: Vec<Disabled>,
}

fn dirty(world: &World) -> Vec<DocKey> {
    world
        .get_resource::<VfxEditorPanel>()
        .and_then(VfxEditorPanel::doc)
        .filter(|doc| doc.dirty())
        .map(|doc| vec![DocKey::new(KIND, Some(Path::new(&doc.path)))])
        .unwrap_or_default()
}

fn snapshot(world: &World) -> Vec<DocSnapshot> {
    let Some(doc) = world
        .get_resource::<VfxEditorPanel>()
        .and_then(VfxEditorPanel::doc)
        .filter(|doc| doc.dirty())
    else {
        return Vec::new();
    };
    match doc.text_to_save().and_then(|text| {
        ron::ser::to_string_pretty(
            &Envelope {
                version: ENVELOPE_VERSION,
                effect: text,
                disabled: doc.state.disabled.clone(),
            },
            regia_core::ron_pretty(),
        )
        .map_err(|e| super::err("serialize_failed", e.to_string()))
    }) {
        Ok(text) => vec![DocSnapshot {
            key: DocKey::new(KIND, Some(Path::new(&doc.path))),
            bytes: text.into_bytes(),
        }],
        Err(err) => {
            tracing::warn!(target: "regia_editor", "VFX autosave: {}", err.message);
            Vec::new()
        }
    }
}

/// The effect text and the stash: an envelope this module writes, or the bare effect text of an
/// autosave from before the stash existed.
fn split(bytes: &[u8]) -> Result<(String, Vec<Disabled>), String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not text: {e}"))?;
    match ron::from_str::<Envelope>(text) {
        Ok(envelope) if envelope.version == ENVELOPE_VERSION => Ok((envelope.effect, envelope.disabled)),
        _ => Ok((text.to_string(), Vec::new())),
    }
}

/// Opens the original effect, then puts the recovered definition and stash over it (dirty).
fn restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let original = key
        .original
        .as_deref()
        .ok_or_else(|| "a VFX autosave needs its file".to_string())?;
    let root: Option<PathBuf> = crate::matgraph_editor::project_root(world);
    let rel = root
        .as_deref()
        .and_then(|root| original.strip_prefix(root).ok())
        .unwrap_or(original)
        .to_string_lossy()
        .replace('\\', "/");
    let (effect, disabled) = split(bytes)?;
    let def = VfxSystemDef::from_ron(&effect).map_err(|e| e.to_string())?;
    super::open_effect(world, &rel).map_err(|e| e.message)?;
    let mut panel = world.get_resource_or_insert_with(VfxEditorPanel::default);
    if let Some(doc) = panel.doc.as_mut() {
        doc.state.def = def;
        doc.state.disabled = disabled;
        doc.revision += 1;
    }
    Ok(())
}

/// The Save all hook (P-147, ADR-8147): the effect editor's Save (validated first, as its Save).
pub(crate) fn save(world: &mut World, _key: &DocKey) -> Result<(), String> {
    if dirty(world).is_empty() {
        return Ok(());
    }
    super::save_effect(world)
        .map(|_| ())
        .map_err(|e| format!("{} ({})", e.message, e.code))
}
