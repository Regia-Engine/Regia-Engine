// SPDX-License-Identifier: MPL-2.0
//! Document autosave of the sequence editor (`crate::doc_autosave`, ADR-9770, ADR-7382): every
//! `.seq.ron` the Sequence Curves panel opened or saved (`SeqCurvePanel::paths`) is a document
//! with its own dirty flag, independent of the level's undo stack.
//!
//! **Dirty flag.** [`SeqSavedCopies`] keeps, per open player, the sequence as it was last read
//! from or written to its file. The document is dirty while the player's sequence differs from
//! that copy: every edit path (the curve, the section strip, a binding rename, undo and redo of
//! either, an agent call) changes the player's sequence, and a save, an open or a take reload
//! records the new copy. Undoing back to the saved sequence is clean again, as it is for the
//! level.
//!
//! **Snapshot.** The text `save_sequence` writes (`Sequence::to_ron`), keyed by the file's path.
//! **Restore** opens the text as a new player at its original path, the way the panel's **Open**
//! does (sub-sequences from disk), with the file on disk (when it reads) as the saved copy, so
//! the document is dirty until a save.
//!
//! The Shots panel's shot list is a document of its own with its own provider (`shots`,
//! `crate::shot_document`, ADR-8730).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use regia_ecs::prelude::*;
use regia_framework::sequencer::{Sequence, SequencePlayer};

use crate::doc_autosave::{DocKey, DocProvider, DocSnapshot};
use crate::seq_curve_panel::{SeqCurvePanel, open_sequence_text, project_root};

/// The provider kind.
pub(crate) const KIND: &str = "sequence";

/// The registry entry.
pub(crate) const PROVIDER: DocProvider = DocProvider {
    kind: KIND,
    dirty,
    snapshot,
    restore,
};

/// The saved copy of each open sequence document, by player: what its file holds as far as this
/// session knows. A player without a copy (a recovered document whose file is gone) is dirty.
#[derive(Resource, Debug, Default, Clone)]
pub struct SeqSavedCopies {
    saved: BTreeMap<Entity, Sequence>,
}

impl SeqSavedCopies {
    /// Records `sequence` as what `player`'s file holds (after an open, a save or a reload).
    pub fn mark_saved(&mut self, player: Entity, sequence: Sequence) {
        self.saved.insert(player, sequence);
    }

    /// Forgets `player`'s saved copy: it is dirty until its next save.
    pub fn forget(&mut self, player: Entity) {
        self.saved.remove(&player);
    }

    /// The saved copy of `player`, when known.
    #[must_use]
    pub fn saved(&self, player: Entity) -> Option<&Sequence> {
        self.saved.get(&player)
    }
}

/// Records `sequence` as `player`'s saved copy.
pub(crate) fn mark_saved(world: &mut World, player: Entity, sequence: Sequence) {
    world
        .get_resource_or_insert_with(SeqSavedCopies::default)
        .mark_saved(player, sequence);
}

/// Whether the sequence document of `player` has unsaved edits.
#[must_use]
pub fn is_dirty(world: &World, player: Entity) -> bool {
    let Some(current) = world.get::<SequencePlayer>(player) else {
        return false;
    };
    world
        .get_resource::<SeqSavedCopies>()
        .and_then(|copies| copies.saved(player))
        != Some(current.sequence())
}

/// The players the Sequence Curves panel opened or saved: documents with their own file, never
/// level content.
#[must_use]
pub fn document_players(world: &World) -> Vec<Entity> {
    world
        .get_resource::<SeqCurvePanel>()
        .map(|panel| panel.paths.keys().copied().collect())
        .unwrap_or_default()
}

/// The open documents with unsaved edits: (player, key), one per file.
fn dirty_players(world: &World) -> Vec<(Entity, DocKey)> {
    let Some(panel) = world.get_resource::<SeqCurvePanel>() else {
        return Vec::new();
    };
    let root = project_root(world);
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (&player, path) in &panel.paths {
        if !is_dirty(world, player) {
            continue;
        }
        let file = root
            .as_deref()
            .map_or_else(|| Path::new(path).to_path_buf(), |root| root.join(path));
        let key = DocKey::new(KIND, Some(&file));
        if seen.insert(key.id()) {
            out.push((player, key));
        }
    }
    out
}

fn dirty(world: &World) -> Vec<DocKey> {
    dirty_players(world).into_iter().map(|(_, key)| key).collect()
}

fn snapshot(world: &World) -> Vec<DocSnapshot> {
    dirty_players(world)
        .into_iter()
        .filter_map(|(player, key)| {
            let sequence = world.get::<SequencePlayer>(player)?.sequence();
            match sequence.to_ron() {
                Ok(text) => Some(DocSnapshot {
                    key,
                    bytes: text.into_bytes(),
                }),
                Err(err) => {
                    tracing::warn!(target: "regia_editor", "sequence autosave of {}: {err}", key.label);
                    None
                }
            }
        })
        .collect()
}

/// `original` as the project-relative `/`-separated path the panel keys documents by.
pub(crate) fn project_relative(world: &World, original: &Path) -> Result<String, String> {
    let root = project_root(world).ok_or_else(|| "no project is open".to_string())?;
    let relative = original
        .strip_prefix(&root)
        .map_err(|_| format!("{} is not in the open project", original.display()))?;
    let parts: Vec<String> = relative
        .components()
        .map(|c| match c {
            Component::Normal(part) => Ok(part.to_string_lossy().into_owned()),
            _ => Err(format!("{} is not a plain project path", original.display())),
        })
        .collect::<Result<_, _>>()?;
    Ok(parts.join("/"))
}

/// Opens the recovered text as a new player at the document's original path: dirty against the
/// file on disk until a save.
fn restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not text: {e}"))?;
    let original = key
        .original
        .as_deref()
        .ok_or_else(|| "a sequence autosave needs its file's path".to_string())?;
    let path = project_relative(world, original)?;
    let mut panel = world.remove_resource::<SeqCurvePanel>().unwrap_or_default();
    let opened = open_sequence_text(world, &mut panel, &path, text);
    world.insert_resource(panel);
    let player = opened.map_err(|e| format!("{}: {}", e.code, e.message))?;
    // The saved copy is what the file holds now (unreadable or gone: none, so dirty).
    let on_disk = std::fs::read_to_string(original)
        .ok()
        .and_then(|text| Sequence::from_ron(&text).ok());
    let mut copies = world.get_resource_or_insert_with(SeqSavedCopies::default);
    match on_disk {
        Some(saved) => copies.mark_saved(player, saved),
        None => copies.forget(player),
    }
    Ok(())
}

/// The Save all hook (P-147, ADR-8147): the Sequence Curves panel's Save of the player whose file
/// the key names (the panel's chosen player is put back after).
pub(crate) fn save(world: &mut World, key: &DocKey) -> Result<(), String> {
    let Some(player) = dirty_players(world)
        .into_iter()
        .find(|(_, dirty)| dirty.id() == key.id())
        .map(|(player, _)| player)
    else {
        return Ok(());
    };
    let mut panel = world.remove_resource::<SeqCurvePanel>().unwrap_or_default();
    let chosen = panel.player.replace(player);
    let saved = crate::seq_curve_panel::save_sequence(world, &mut panel, None);
    panel.player = chosen;
    world.insert_resource(panel);
    saved
        .map(|_| ())
        .map_err(|e| format!("{} ({})", e.message, e.code))
}
