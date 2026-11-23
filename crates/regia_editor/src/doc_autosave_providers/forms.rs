// SPDX-License-Identifier: MPL-2.0
//! Document autosave of five asset editors (sprint w11-forms-undo-autosave; `crate::doc_autosave`,
//! ADR-9770): the cloth panel (`cloth`), the Ragdoll editor (`ragdoll`), Vehicle Tuning (`vehicle`),
//! the Abilities inspector's asset editor (`abilities`) and the Localization panel's edit box (`loc`).
//! Each snapshot is the bytes the editor's save writes; each restore opens the original file in the
//! editor and puts the recovered edit over it, unsaved.

use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::doc_autosave::{DocKey, DocProvider, DocSnapshot};

/// The project root the editors resolve their paths against.
fn root(world: &World) -> Option<PathBuf> {
    crate::mesh_source::project_root(world)
}

/// `rel` under `base` as the key's original (an absolute path when the project is open).
fn original(base: Option<PathBuf>, rel: &str) -> PathBuf {
    base.map_or_else(|| PathBuf::from(rel), |b| b.join(rel))
}

/// The key's original as a path relative to `base` with `/` separators.
fn relative(base: Option<&Path>, key: &DocKey, kind: &str) -> Result<String, String> {
    let original = key
        .original
        .as_deref()
        .ok_or_else(|| format!("a {kind} autosave needs its file"))?;
    Ok(base
        .and_then(|b| original.strip_prefix(b).ok())
        .unwrap_or(original)
        .to_string_lossy()
        .replace('\\', "/"))
}

fn text(bytes: &[u8]) -> Result<&str, String> {
    std::str::from_utf8(bytes).map_err(|e| format!("not text: {e}"))
}

fn one(key: DocKey, text: Result<String, String>, kind: &str) -> Vec<DocSnapshot> {
    match text {
        Ok(text) => vec![DocSnapshot {
            key,
            bytes: text.into_bytes(),
        }],
        Err(err) => {
            tracing::warn!(target: "regia_editor", "{kind} autosave: {err}");
            Vec::new()
        }
    }
}

// ------------------------------------------------------------------------------------------- cloth

/// The cloth panel's kind.
pub(crate) const CLOTH_KIND: &str = "cloth";

/// The cloth panel's registry entry: the open `.cloth.ron` / `.soft.ron` while it has unsaved edits.
pub(crate) const CLOTH: DocProvider = DocProvider {
    kind: CLOTH_KIND,
    dirty: cloth_dirty,
    snapshot: cloth_snapshot,
    restore: cloth_restore,
};

fn cloth_doc(world: &World) -> Option<&crate::cloth_panel::ClothDoc> {
    world
        .get_resource::<crate::cloth_panel::ClothPanel>()
        .and_then(crate::cloth_panel::ClothPanel::doc)
        .filter(|doc| doc.dirty())
}

fn cloth_dirty(world: &World) -> Vec<DocKey> {
    cloth_doc(world)
        .map(|doc| vec![DocKey::new(CLOTH_KIND, Some(&original(root(world), &doc.path)))])
        .unwrap_or_default()
}

fn cloth_snapshot(world: &World) -> Vec<DocSnapshot> {
    let Some(doc) = cloth_doc(world) else {
        return Vec::new();
    };
    let key = DocKey::new(CLOTH_KIND, Some(&original(root(world), &doc.path)));
    one(key, doc.save_text().map_err(|e| e.message), CLOTH_KIND)
}

/// Opens the original file in the cloth panel, then puts the recovered source over it (unsaved).
fn cloth_restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let rel = relative(root(world).as_deref(), key, CLOTH_KIND)?;
    let source = crate::cloth_panel::parse_source(&rel, text(bytes)?).map_err(|e| e.message)?;
    crate::cloth_panel::open_file(world, &rel).map_err(|e| e.message)?;
    let mut panel = world.resource_mut::<crate::cloth_panel::ClothPanel>();
    if let Some(doc) = panel.doc_mut() {
        doc.source = source;
    }
    Ok(())
}

// ----------------------------------------------------------------------------------------- ragdoll

/// The Ragdoll editor's kind.
pub(crate) const RAGDOLL_KIND: &str = "ragdoll";

/// The Ragdoll editor's registry entry: the open profile while it has unsaved edits (a new profile
/// not saved yet too: it has its path).
pub(crate) const RAGDOLL: DocProvider = DocProvider {
    kind: RAGDOLL_KIND,
    dirty: ragdoll_dirty,
    snapshot: ragdoll_snapshot,
    restore: ragdoll_restore,
};

fn ragdoll_open(world: &World) -> Option<(&str, &regia_framework::ragdoll::RagdollProfile)> {
    let panel = world.get_resource::<crate::ragdoll_panel::RagdollPanel>()?;
    if !panel.dirty {
        return None;
    }
    Some((panel.path.as_deref()?, panel.profile.as_ref()?))
}

fn ragdoll_dirty(world: &World) -> Vec<DocKey> {
    ragdoll_open(world)
        .map(|(path, _)| vec![DocKey::new(RAGDOLL_KIND, Some(&original(root(world), path)))])
        .unwrap_or_default()
}

fn ragdoll_snapshot(world: &World) -> Vec<DocSnapshot> {
    let Some((path, profile)) = ragdoll_open(world) else {
        return Vec::new();
    };
    let key = DocKey::new(RAGDOLL_KIND, Some(&original(root(world), path)));
    one(key, profile.to_ron().map_err(|e| e.to_string()), RAGDOLL_KIND)
}

/// Opens the original profile (or starts a new one at its path when it was never saved), then puts
/// the recovered profile over it (dirty).
fn ragdoll_restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let project = root(world);
    let rel = relative(project.as_deref(), key, RAGDOLL_KIND)?;
    let profile =
        regia_framework::ragdoll::RagdollProfile::from_ron(text(bytes)?).map_err(|e| e.to_string())?;
    let mut panel = world.get_resource_or_insert_with(crate::ragdoll_panel::RagdollPanel::default);
    let exists = project.as_deref().is_some_and(|p| p.join(&rel).is_file());
    if exists {
        panel.open(project.as_deref(), &rel).map_err(|e| e.message)?;
    } else {
        panel.new_profile(&rel).map_err(|e| e.message)?;
    }
    panel.profile = Some(profile);
    panel.dirty = true;
    panel.generation += 1;
    Ok(())
}

// ----------------------------------------------------------------------------------------- vehicle

/// Vehicle Tuning's kind.
pub(crate) const VEHICLE_KIND: &str = "vehicle";

/// Vehicle Tuning's registry entry: the open `.vehicle.ron` while it has unsaved edits.
pub(crate) const VEHICLE: DocProvider = DocProvider {
    kind: VEHICLE_KIND,
    dirty: vehicle_dirty,
    snapshot: vehicle_snapshot,
    restore: vehicle_restore,
};

fn vehicle_open(world: &World) -> Option<&crate::vehicle_tuning::VehicleTuning> {
    world
        .get_resource::<crate::vehicle_tuning::VehicleTuning>()
        .filter(|t| t.is_dirty() && t.path.is_some())
}

fn vehicle_dirty(world: &World) -> Vec<DocKey> {
    vehicle_open(world)
        .and_then(|t| t.path.as_deref())
        .map(|path| vec![DocKey::new(VEHICLE_KIND, Some(&original(root(world), path)))])
        .unwrap_or_default()
}

fn vehicle_snapshot(world: &World) -> Vec<DocSnapshot> {
    let Some(tuning) = vehicle_open(world) else {
        return Vec::new();
    };
    let (Some(path), Some(asset)) = (tuning.path.as_deref(), tuning.asset.as_ref()) else {
        return Vec::new();
    };
    let key = DocKey::new(VEHICLE_KIND, Some(&original(root(world), path)));
    let text = crate::vehicle_tuning::save_text(tuning.source.as_deref(), tuning.opened.as_ref(), asset)
        .map(|(text, _)| text)
        .map_err(|e| e.message);
    one(key, text, VEHICLE_KIND)
}

/// Vehicle Tuning's retained vehicles (P-322 part b): a `.vehicle.ron` opened before the one the
/// panel shows, while it has unsaved edits (restored through [`VEHICLE`]).
pub(crate) const VEHICLE_RETAINED: crate::doc_autosave::RetainedProvider =
    crate::doc_autosave::RetainedProvider {
        kind: VEHICLE_KIND,
        dirty: vehicle_retained_dirty,
        snapshot: vehicle_retained_snapshot,
    };

fn vehicle_retained_dirty(world: &World) -> Vec<DocKey> {
    let project = root(world);
    crate::vehicle_tuning::retained_dirty_vehicles(world)
        .into_iter()
        .map(|doc| DocKey::new(VEHICLE_KIND, Some(&original(project.clone(), doc.path))))
        .collect()
}

fn vehicle_retained_snapshot(world: &World) -> Vec<DocSnapshot> {
    let project = root(world);
    crate::vehicle_tuning::retained_dirty_vehicles(world)
        .into_iter()
        .flat_map(|doc| {
            let key = DocKey::new(VEHICLE_KIND, Some(&original(project.clone(), doc.path)));
            let text = crate::vehicle_tuning::save_text(doc.source, doc.opened, doc.asset)
                .map(|(text, _)| text)
                .map_err(|e| e.message);
            one(key, text, VEHICLE_KIND)
        })
        .collect()
}

/// Opens the original file (a new one at its path when it was never saved), then puts the recovered
/// asset over it (unsaved).
fn vehicle_restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let project = root(world);
    let rel = relative(project.as_deref(), key, VEHICLE_KIND)?;
    let asset =
        regia::physics::vehicle_asset::VehicleAsset::from_ron(text(bytes)?).map_err(|e| e.to_string())?;
    let mut tuning = world.get_resource_or_insert_with(crate::vehicle_tuning::VehicleTuning::default);
    let exists = project.as_deref().is_some_and(|p| p.join(&rel).is_file());
    tuning
        .open(project.as_deref(), &rel, !exists)
        .map_err(|e| e.message)?;
    tuning.asset = Some(asset);
    Ok(())
}

// --------------------------------------------------------------------------------------- abilities

/// The Abilities asset editor's kind.
pub(crate) const ABILITIES_KIND: &str = "abilities";

/// The Abilities inspector's asset editor's registry entry: the open `.attrs.ron`, `.effect.ron` or
/// `.ability.ron` while its text differs from the file.
pub(crate) const ABILITIES: DocProvider = DocProvider {
    kind: ABILITIES_KIND,
    dirty: abilities_dirty,
    snapshot: abilities_snapshot,
    restore: abilities_restore,
};

fn abilities_doc(world: &World) -> Option<&crate::abilities_panel::AssetDoc> {
    world
        .get_resource::<crate::abilities_panel::AbilitiesPanel>()
        .and_then(|p| p.asset.as_ref())
        .filter(|doc| doc.text != doc.saved)
}

/// The asset editor's paths are under `assets/`.
fn assets_dir(world: &World) -> Option<PathBuf> {
    root(world).map(|r| r.join("assets"))
}

fn abilities_dirty(world: &World) -> Vec<DocKey> {
    abilities_doc(world)
        .map(|doc| {
            vec![DocKey::new(
                ABILITIES_KIND,
                Some(&original(assets_dir(world), &doc.path)),
            )]
        })
        .unwrap_or_default()
}

fn abilities_snapshot(world: &World) -> Vec<DocSnapshot> {
    let Some(doc) = abilities_doc(world) else {
        return Vec::new();
    };
    let key = DocKey::new(ABILITIES_KIND, Some(&original(assets_dir(world), &doc.path)));
    one(key, Ok(doc.text.clone()), ABILITIES_KIND)
}

/// Opens the original asset in the asset editor, then puts the recovered text over it (unsaved,
/// validated).
fn abilities_restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let project = root(world);
    let rel = relative(assets_dir(world).as_deref(), key, ABILITIES_KIND)?;
    let recovered = text(bytes)?.to_owned();
    let mut panel = world
        .remove_resource::<crate::abilities_panel::AbilitiesPanel>()
        .unwrap_or_default();
    let result = panel
        .open_asset(world, project.as_deref(), &rel)
        .and_then(|()| panel.set_asset_text(world, project.as_deref(), &recovered))
        .map_err(|e| e.message);
    world.insert_resource(panel);
    result
}

// --------------------------------------------------------------------------------------------- loc

/// The Localization panel's kind.
pub(crate) const LOC_KIND: &str = "loc";

/// The Localization panel's registry entry. The panel writes each translation when it is applied, so
/// its one unsaved edit is the edit box: the selected cell's text typed but not applied yet.
pub(crate) const LOC: DocProvider = DocProvider {
    kind: LOC_KIND,
    dirty: loc_dirty,
    snapshot: loc_snapshot,
    restore: loc_restore,
};

/// The selected cell and the edit box's text, when the text differs from what the panel shows for
/// the cell (its text, else the source text: what selecting it puts in the box).
fn loc_edit(world: &World) -> Option<(&str, &str, &str)> {
    let panel = world.get_resource::<crate::loc_panel::LocPanel>()?;
    let (key, locale) = panel.selected.as_ref()?;
    let table = panel.table()?;
    let row = table.rows.iter().find(|r| &r.key == key);
    let shown = row
        .and_then(|r| r.cells.get(locale))
        .and_then(|c| c.text.as_deref())
        .or_else(|| {
            row.and_then(|r| r.cells.get(&table.source))
                .and_then(|c| c.text.as_deref())
        })
        .unwrap_or_default();
    (panel.edit_text != shown).then_some((key.as_str(), locale.as_str(), panel.edit_text.as_str()))
}

fn loc_key(key: &str, locale: &str) -> DocKey {
    let mut doc = DocKey::new(LOC_KIND, None);
    doc.label = format!("{key} ({locale})");
    doc
}

fn loc_dirty(world: &World) -> Vec<DocKey> {
    loc_edit(world)
        .map(|(key, locale, _)| vec![loc_key(key, locale)])
        .unwrap_or_default()
}

fn loc_snapshot(world: &World) -> Vec<DocSnapshot> {
    let Some((key, locale, text)) = loc_edit(world) else {
        return Vec::new();
    };
    let bytes = serde_json::to_vec_pretty(&json!({ "key": key, "locale": locale, "text": text }))
        .map_err(|e| e.to_string());
    match bytes {
        Ok(bytes) => vec![DocSnapshot {
            key: loc_key(key, locale),
            bytes,
        }],
        Err(err) => {
            tracing::warn!(target: "regia_editor", "loc autosave: {err}");
            Vec::new()
        }
    }
}

/// Selects the cell again and puts the recovered text in the edit box (Apply writes it).
fn loc_restore(world: &mut World, _key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| format!("not a loc autosave: {e}"))?;
    let field = |name: &str| {
        value[name]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("the loc autosave has no `{name}`"))
    };
    let (key, locale, text) = (field("key")?, field("locale")?, field("text")?);
    let mut panel = world.get_resource_or_insert_with(crate::loc_panel::LocPanel::default);
    panel.selected = Some((key, locale));
    panel.edit_text = text;
    panel.reload = true;
    Ok(())
}

// ------------------------------------------------------------------------------- save hooks (P-147)

/// The cloth panel's Save all hook (ADR-8147): its Save.
pub(crate) fn cloth_save(world: &mut World, _key: &DocKey) -> Result<(), String> {
    crate::cloth_panel::save_file(world)
        .map(|_| ())
        .map_err(|e| format!("{} ({})", e.message, e.code))
}

/// The Ragdoll editor's Save all hook (ADR-8147): its Save of the open profile.
pub(crate) fn ragdoll_save(world: &mut World, _key: &DocKey) -> Result<(), String> {
    if ragdoll_open(world).is_none() {
        return Ok(());
    }
    let project = root(world);
    let mut panel = world
        .remove_resource::<crate::ragdoll_panel::RagdollPanel>()
        .unwrap_or_default();
    let saved = panel.save(project.as_deref());
    panel.error = saved.as_ref().err().cloned();
    world.insert_resource(panel);
    saved
        .map(|_| ())
        .map_err(|e| format!("{} ({})", e.message, e.code))
}

/// The vehicle tuning panel's Save all hook (ADR-8147): its Save to the open file.
pub(crate) fn vehicle_save(world: &mut World, _key: &DocKey) -> Result<(), String> {
    if vehicle_dirty(world).is_empty() {
        return Ok(());
    }
    crate::vehicle_tuning::with_panel(world, |tuning, project| tuning.save(project, None))
        .map(|_| ())
        .map_err(|e| format!("{} ({})", e.message, e.code))
}

/// The Abilities asset editor's Save all hook (ADR-8147): its Save (refused while the asset has
/// errors, as its Save).
pub(crate) fn abilities_save(world: &mut World, _key: &DocKey) -> Result<(), String> {
    if abilities_doc(world).is_none() {
        return Ok(());
    }
    let project = root(world);
    let mut panel = world
        .remove_resource::<crate::abilities_panel::AbilitiesPanel>()
        .unwrap_or_default();
    let saved = panel.save_asset(project.as_deref());
    world.insert_resource(panel);
    saved
        .map(|_| ())
        .map_err(|e| format!("{} ({})", e.message, e.code))
}

/// The Localization panel's Save all hook (ADR-8147): the edit box's text is applied to its cell
/// (written to the string table, one undo step), then the panel reads the files again.
pub(crate) fn loc_save(world: &mut World, _key: &DocKey) -> Result<(), String> {
    let Some((key, locale, text)) =
        loc_edit(world).map(|(k, l, t)| (k.to_owned(), l.to_owned(), t.to_owned()))
    else {
        return Ok(());
    };
    crate::loc_panel::set_translation(world, &key, &locale, &text)
        .map_err(|e| format!("{} ({})", e.message, e.code))?;
    world
        .get_resource_or_insert_with(crate::loc_panel::LocPanel::default)
        .reload = true;
    crate::loc_panel::with_panel(world, |_, _| Ok(())).map_err(|e| format!("{} ({})", e.message, e.code))
}

#[cfg(test)]
mod tests;
