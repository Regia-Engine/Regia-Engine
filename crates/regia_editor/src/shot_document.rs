// SPDX-License-Identifier: MPL-2.0
//! The Shots panel's shot list as a document (ADR-8730, `book/agents/shot-panel.md`): the
//! `ShotPlayer`'s loaded `.shots.ron` with its project path, its saved copy and a dirty flag,
//! plus Open, Save and its document autosave provider (`crate::doc_autosave`).
//!
//! **Where the state lives.** [`ShotDocument`], one editor resource next to the `ShotPlayer`
//! resource (there is one shot player, so one shot-list document): the project-relative path the
//! list was opened from or last saved to, and the list as that file holds it. The runtime player
//! stays a plain player; nothing here is saved with the level.
//!
//! **Dirty flag.** The document is dirty while the player's list differs from the saved copy.
//! Every edit path (the Shots panel's curve, the timeline's cuts and fades, undo and redo of
//! either, an agent call) changes the player's list, and a save or an open records the new copy,
//! so undoing back to the saved list is clean again. A list loaded without a document
//! (`render.sequence`, the timeline's first **Add cut**) has no saved copy: it is dirty and has no
//! path until a save gives it one.
//!
//! **Open** replaces the loaded list and starts a new document generation
//! ([`ShotDocument::generation`]): an undo entry of a cut edit made on the previous list does not
//! apply to the new one (`crate::seq_timeline`).
//!
//! **Autosave** (provider `shots`): a dirty document with a path snapshots the text a save
//! writes (the current format version, as Save writes it); Restore loads it back into the
//! `ShotPlayer` at the original path, with the file on disk (when it reads) as the saved copy, so
//! it is dirty until a save. A list adopted from a sequence player and saved to a file is restored
//! into the `ShotPlayer` too: the recovery row keeps the file, not the host player.
//!
//! **Detach (ADR-8940).** The document remembers the `ShotPlayer`'s load count
//! (`ShotPlayer::load_count`) when its list went in. Another list loaded into the player
//! (`render.sequence { action: "load" }`, `camera.set_path` on an empty player, a host's `load`
//! or `clear`) detaches it: the path is cleared, the new list has no saved copy (so it is dirty),
//! the generation moves on, and the Shots panel's path row is emptied when it still named the old
//! file. Save then needs a path: without one it answers `shot_panel_detached`. Every read
//! (`path`, `is_dirty`, `generation`) sees the detach at once; `sync` commits it.
//!
//! **Host (ADR-8940).** The document's list lives in the `ShotPlayer` (opened, restored, loaded
//! elsewhere) or in a sequence player's own shot list (`SequencePlayer::set_shots`), which
//! [`adopt_player_shots`] takes as an unsaved document. Save As gives it a file (at the current
//! format version); after that it behaves like an opened document. The Shots panel's curve and the
//! timeline's cut row edit whichever list the document holds. A host player that despawns detaches
//! the document back to the `ShotPlayer`.

use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use regia_framework::sequencer::SequencePlayer;
use regia_framework::shots::{SHOT_SEQUENCE_FORMAT_VERSION, ShotPlayer, ShotSequence};

use crate::doc_autosave::{DocKey, DocProvider, DocSnapshot};
use crate::shot_panel::PanelError;

/// The autosave provider kind.
pub(crate) const KIND: &str = "shots";

/// The autosave registry entry.
pub(crate) const PROVIDER: DocProvider = DocProvider {
    kind: KIND,
    dirty,
    snapshot,
    restore,
};

/// The Save all hook (P-147, ADR-8147): the Shots panel's Save to the list's file.
pub(crate) fn save_hook(world: &mut World, _key: &DocKey) -> Result<(), String> {
    if !is_dirty(world) {
        return Ok(());
    }
    save_shot_list(world, None)
        .map(|_| ())
        .map_err(|e| format!("{} ({})", e.message, e.code))
}

/// The file suffix of a shot list.
pub const SHOTS_SUFFIX: &str = ".shots.ron";

/// Where the document's shot list lives (ADR-8940).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShotHost {
    /// The `ShotPlayer` resource's loaded sequence.
    #[default]
    ShotPlayer,
    /// A sequence player's own shot list (`SequencePlayer::shots`).
    Player(Entity),
}

/// The shot-list document: where its list lives, its file and what that file holds.
#[derive(Resource, Debug, Default, Clone)]
pub struct ShotDocument {
    path: Option<String>,
    saved: Option<ShotSequence>,
    generation: u64,
    host: ShotHost,
    /// The `ShotPlayer`'s load count when the document's list went in (`None`: no list of the
    /// `ShotPlayer` belongs to the document).
    loads: Option<u64>,
    /// The file of the last document a detach cleared (named by `shot_panel_detached`).
    detached_from: Option<String>,
}

impl ShotDocument {
    /// The project-relative `.shots.ron` the list was opened from or last saved to.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// The list as its file holds it, as far as this session knows.
    #[must_use]
    pub fn saved(&self) -> Option<&ShotSequence> {
        self.saved.as_ref()
    }

    /// Counts opens, restores, adoptions and detaches: a new list is a new generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Where the document's list lives.
    #[must_use]
    pub fn host(&self) -> ShotHost {
        self.host
    }
}

fn refuse<T>(code: &'static str, message: impl Into<String>) -> Result<T, PanelError> {
    Err(PanelError {
        code,
        message: message.into(),
    })
}

/// Whether the document no longer holds the list in its host: the `ShotPlayer` was loaded or
/// cleared by someone else since the document's list went in, or its host player is gone.
fn stale(world: &World) -> bool {
    let Some(doc) = world.get_resource::<ShotDocument>() else {
        return false;
    };
    match doc.host {
        ShotHost::ShotPlayer => doc.loads.is_some_and(|seen| {
            world
                .get_resource::<ShotPlayer>()
                .map_or(0, ShotPlayer::load_count)
                != seen
        }),
        ShotHost::Player(e) => world.get::<SequencePlayer>(e).is_none(),
    }
}

/// Where the document's list lives now (the `ShotPlayer` once a host player is gone).
#[must_use]
pub fn host(world: &World) -> ShotHost {
    match world.get_resource::<ShotDocument>().map(|d| d.host) {
        Some(ShotHost::Player(e)) if world.get::<SequencePlayer>(e).is_some() => ShotHost::Player(e),
        _ => ShotHost::ShotPlayer,
    }
}

/// The document's list: the `ShotPlayer`'s loaded one, or the host player's own.
#[must_use]
pub fn loaded(world: &World) -> Option<&ShotSequence> {
    match host(world) {
        ShotHost::ShotPlayer => world.get_resource::<ShotPlayer>().and_then(ShotPlayer::sequence),
        ShotHost::Player(e) => world.get::<SequencePlayer>(e).and_then(SequencePlayer::shots),
    }
}

/// Puts an edited version of the document's list into its host (the `ShotPlayer` keeps its clock
/// and play state).
pub fn put(world: &mut World, list: ShotSequence) {
    match host(world) {
        ShotHost::ShotPlayer => {
            if let Some(mut player) = world.get_resource_mut::<ShotPlayer>() {
                player.replace_sequence(list);
            }
        }
        ShotHost::Player(e) => {
            if let Some(mut player) = world.get_mut::<SequencePlayer>(e) {
                player.set_shots(Some(list));
            }
        }
    }
}

/// The document generation (0 before any open or restore); a pending detach counts.
#[must_use]
pub fn generation(world: &World) -> u64 {
    world
        .get_resource::<ShotDocument>()
        .map_or(0, |d| d.generation + u64::from(stale(world)))
}

/// The document's project-relative path, when it has one (none once detached).
#[must_use]
pub fn path(world: &World) -> Option<String> {
    if stale(world) {
        return None;
    }
    world.get_resource::<ShotDocument>().and_then(|d| d.path.clone())
}

/// Whether the loaded shot list has unsaved edits (false with no list loaded; true for a list
/// with no saved copy, a detached one included).
#[must_use]
pub fn is_dirty(world: &World) -> bool {
    let Some(current) = loaded(world) else {
        return false;
    };
    let saved = if stale(world) {
        None
    } else {
        world.get_resource::<ShotDocument>().and_then(ShotDocument::saved)
    };
    saved != Some(current)
}

/// Commits a pending detach (see the module docs): clears the path and the saved copy, moves the
/// generation on, and empties the Shots panel's path row when it still names the old file.
/// Returns the old file's path when it detached a document that had one.
pub fn sync(world: &mut World) -> Option<String> {
    if !stale(world) {
        return None;
    }
    let mut doc = world.get_resource_mut::<ShotDocument>()?;
    let old = doc.path.take();
    doc.saved = None;
    doc.generation += 1;
    doc.loads = None;
    doc.host = ShotHost::ShotPlayer;
    if old.is_some() {
        doc.detached_from.clone_from(&old);
    }
    if let Some(mut panel) = world.get_resource_mut::<crate::shot_panel::ShotPanel>()
        && old.as_deref() == Some(panel.path_text.trim())
    {
        panel.path_text.clear();
    }
    tracing::debug!(target: "regia_editor", "shot list document detached from {old:?}");
    old
}

/// `path` under the project root, ending in `.shots.ron`. Codes: `shot_panel_bad_path`,
/// `shot_panel_no_project`.
fn project_file(world: &World, path: &str) -> Result<PathBuf, PanelError> {
    crate::seq_curve_panel::project_file_with(world, path, SHOTS_SUFFIX).map_err(|e| PanelError {
        code: if e.code == "seq_curve_no_project" {
            "shot_panel_no_project"
        } else {
            "shot_panel_bad_path"
        },
        message: e.message,
    })
}

/// Loads `list` into the `ShotPlayer` (stopped at zero) as the document at `path` with `saved`
/// as its file's contents, starting a new generation.
fn adopt(world: &mut World, path: &str, list: ShotSequence, saved: Option<ShotSequence>) {
    // A pending detach first, so the new list's generation is past every older one.
    sync(world);
    let mut player = world.get_resource_or_insert_with(ShotPlayer::default);
    player.load(list);
    let loads = player.load_count();
    let mut doc = world.get_resource_or_insert_with(ShotDocument::default);
    doc.path = Some(path.to_string());
    doc.saved = saved;
    doc.generation += 1;
    doc.host = ShotHost::ShotPlayer;
    doc.loads = Some(loads);
    doc.detached_from = None;
    if let Some(mut panel) = world.get_resource_mut::<crate::shot_panel::ShotPanel>() {
        panel.shot = 0;
        panel.selected = None;
        panel.error = None;
        panel.path_text = path.to_string();
    }
}

/// Opens the project `.shots.ron` at `path` into the `ShotPlayer` (stopped at zero) as the
/// document. Refused while the loaded list has unsaved edits unless `discard`. Returns the
/// number of shots. Codes: `shot_panel_unsaved`, `shot_panel_bad_path`, `shot_panel_no_project`,
/// `shot_panel_io`, `shot_panel_bad_file`.
pub fn open_shot_list(world: &mut World, path: &str, discard: bool) -> Result<usize, PanelError> {
    sync(world);
    if !discard && is_dirty(world) {
        return refuse(
            "shot_panel_unsaved",
            "the loaded shot list has unsaved edits: save it first, or open with discard",
        );
    }
    let file = project_file(world, path)?;
    let text = std::fs::read_to_string(&file).map_err(|e| PanelError {
        code: "shot_panel_io",
        message: format!("cannot read {path}: {e}"),
    })?;
    let list = ShotSequence::from_ron(&text).map_err(|e| PanelError {
        code: "shot_panel_bad_file",
        message: format!("{path}: {e}"),
    })?;
    let shots = list.shots.len();
    adopt(world, path, list.clone(), Some(list));
    Ok(shots)
}

/// Saves the document's shot list to `path` (default: the document's path) with an atomic write
/// and makes that the document's path. The file carries the current format version whatever
/// version number the list in memory has (a host may build one in code). Returns the path and the
/// bytes written. A failed save changes nothing (the document stays dirty). A read-only
/// destination file is replaced (the shared atomic writer clears the flag); a folder that cannot
/// be written answers `shot_panel_io`. Codes: `shot_panel_no_sequence`, `shot_panel_detached` (no
/// path, and the document was detached from its file), `shot_panel_bad_path`,
/// `shot_panel_no_project`, `shot_panel_invalid`, `shot_panel_io`.
pub fn save_shot_list(world: &mut World, path: Option<&str>) -> Result<(String, usize), PanelError> {
    sync(world);
    let Some(list) = loaded(world).cloned() else {
        return refuse("shot_panel_no_sequence", "no shot sequence is loaded");
    };
    let Some(path) = path.map(str::to_string).or_else(|| self::path(world)) else {
        if let Some(old) = world
            .get_resource::<ShotDocument>()
            .and_then(|d| d.detached_from.clone())
        {
            return refuse(
                "shot_panel_detached",
                format!(
                    "another shot list replaced the one opened from {old}, so Save no longer writes \
                     there: give a project path ending in .shots.ron"
                ),
            );
        }
        return refuse(
            "shot_panel_bad_path",
            "the shot list has no file yet: give a project path ending in .shots.ron",
        );
    };
    let file = project_file(world, &path)?;
    let invalid = |e: regia_framework::shots::ShotSequenceError| PanelError {
        code: "shot_panel_invalid",
        message: e.to_string(),
    };
    list.validate().map_err(invalid)?;
    let mut current = list.clone();
    current.version = SHOT_SEQUENCE_FORMAT_VERSION;
    let text = current.to_ron().map_err(invalid)?;
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| PanelError {
            code: "shot_panel_io",
            message: format!("cannot create {}: {e}", parent.display()),
        })?;
    }
    crate::level_io::write_atomic(&file, text.as_bytes()).map_err(|message| PanelError {
        code: "shot_panel_io",
        message,
    })?;
    let host = host(world);
    let loads = world.get_resource::<ShotPlayer>().map(ShotPlayer::load_count);
    let mut doc = world.get_resource_or_insert_with(ShotDocument::default);
    doc.path = Some(path.clone());
    doc.saved = Some(list);
    doc.detached_from = None;
    if host == ShotHost::ShotPlayer {
        // A list loaded without a document (`render.sequence`) becomes the document here.
        doc.loads = loads;
    }
    if let Some(mut panel) = world.get_resource_mut::<crate::shot_panel::ShotPanel>() {
        panel.path_text = path.clone();
    }
    Ok((path, text.len()))
}

/// Takes the shot list of the sequence player `player` (set by the host with
/// `SequencePlayer::set_shots`) as the document: unsaved, no path, a new generation. Save As then
/// gives it a file. Refused while the current document has unsaved edits unless `discard`.
/// Returns the number of shots. Codes: `shot_panel_no_player`, `shot_panel_no_sequence`,
/// `shot_panel_unsaved`.
pub fn adopt_player_shots(world: &mut World, player: Entity, discard: bool) -> Result<usize, PanelError> {
    sync(world);
    let Some(owner) = world.get::<SequencePlayer>(player) else {
        return refuse(
            "shot_panel_no_player",
            format!("entity {} is not a sequence player", player.to_bits()),
        );
    };
    let Some(shots) = owner.shots().map(|s| s.shots.len()) else {
        return refuse(
            "shot_panel_no_sequence",
            format!("sequence player {} has no shot list of its own", player.to_bits()),
        );
    };
    let same = host(world) == ShotHost::Player(player);
    if !same && !discard && is_dirty(world) {
        return refuse(
            "shot_panel_unsaved",
            "the loaded shot list has unsaved edits: save it first, or adopt with discard",
        );
    }
    let mut doc = world.get_resource_or_insert_with(ShotDocument::default);
    doc.path = None;
    doc.saved = None;
    doc.generation += 1;
    doc.host = ShotHost::Player(player);
    doc.loads = None;
    doc.detached_from = None;
    if let Some(mut panel) = world.get_resource_mut::<crate::shot_panel::ShotPanel>() {
        panel.shot = 0;
        panel.selected = None;
        panel.error = None;
        panel.path_text.clear();
    }
    Ok(shots)
}

// ---------------------------------------------------------------- autosave

/// The dirty document's autosave key: only a document with a file has a place to restore to.
fn dirty_key(world: &World) -> Option<DocKey> {
    if !is_dirty(world) {
        return None;
    }
    let path = self::path(world)?;
    let file = crate::seq_curve_panel::project_root(world)
        .map_or_else(|| Path::new(&path).to_path_buf(), |root| root.join(&path));
    Some(DocKey::new(KIND, Some(&file)))
}

fn dirty(world: &World) -> Vec<DocKey> {
    dirty_key(world).into_iter().collect()
}

fn snapshot(world: &World) -> Vec<DocSnapshot> {
    let Some(key) = dirty_key(world) else {
        return Vec::new();
    };
    let Some(list) = loaded(world) else {
        return Vec::new();
    };
    // The text a save writes: the current format version whatever number the list in memory has
    // (a host may build one in code), so Restore reads it as what it is.
    let mut list = list.clone();
    list.version = SHOT_SEQUENCE_FORMAT_VERSION;
    match list.to_ron() {
        Ok(text) => vec![DocSnapshot {
            key,
            bytes: text.into_bytes(),
        }],
        Err(err) => {
            tracing::warn!(target: "regia_editor", "shot list autosave of {}: {err}", key.label);
            Vec::new()
        }
    }
}

/// Loads the recovered text into the `ShotPlayer` as the document at its original path: dirty
/// against the file on disk until a save. A list that lived in a sequence player's own shot list
/// (adopted, then saved to a file) comes back in the `ShotPlayer` too, not in that player: the
/// autosave keeps the file, not the host.
fn restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not text: {e}"))?;
    let original = key
        .original
        .as_deref()
        .ok_or_else(|| "a shot list autosave needs its file's path".to_string())?;
    let path = crate::seq_autosave::project_relative(world, original)?;
    project_file(world, &path).map_err(|e| format!("{}: {}", e.code, e.message))?;
    let list = ShotSequence::from_ron(text).map_err(|e| format!("shot_panel_bad_file: {e}"))?;
    let on_disk = std::fs::read_to_string(original)
        .ok()
        .and_then(|text| ShotSequence::from_ron(&text).ok());
    adopt(world, &path, list, on_disk);
    Ok(())
}

#[cfg(test)]
mod tests;
