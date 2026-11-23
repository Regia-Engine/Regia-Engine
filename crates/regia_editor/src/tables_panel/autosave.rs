// SPDX-License-Identifier: MPL-2.0
//! Document autosave of the Tables panel (`crate::doc_autosave`, ADR-9770): the open curve table,
//! chooser and data table, each as its save writes it.

use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;

use super::{SchemaLookup, TablesPanel};
use crate::doc_autosave::{DocKey, DocProvider, DocSnapshot, RetainedProvider};

/// The provider kind.
pub(crate) const KIND: &str = "table";

/// The registry entry.
pub(crate) const PROVIDER: DocProvider = DocProvider {
    kind: KIND,
    dirty,
    snapshot,
    restore,
};

/// The key of a table at `path` under `assets/` (kept project-relative).
fn key(path: &str) -> DocKey {
    DocKey::new(KIND, Some(&Path::new("assets").join(path)))
}

fn dirty_paths(panel: &TablesPanel) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(doc) = panel.curves.as_ref().filter(|d| d.dirty) {
        out.push(doc.path.clone());
    }
    if let Some(doc) = panel.chooser.as_ref().filter(|d| d.dirty) {
        out.push(doc.path.clone());
    }
    if let Some(doc) = panel.data.as_ref().filter(|d| d.dirty) {
        out.push(doc.path.clone());
    }
    out
}

fn dirty(world: &World) -> Vec<DocKey> {
    world
        .get_resource::<TablesPanel>()
        .map(|panel| dirty_paths(panel).iter().map(|p| key(p)).collect())
        .unwrap_or_default()
}

fn curve_snapshot(doc: &super::CurveDoc) -> Option<DocSnapshot> {
    let text = if doc.path.to_ascii_lowercase().ends_with(".csv") {
        doc.table.to_csv_string().map_err(|e| e.to_string())
    } else {
        Ok(doc.table.to_ron_string())
    };
    match text {
        Ok(text) => Some(DocSnapshot {
            key: key(&doc.path),
            bytes: text.into_bytes(),
        }),
        Err(err) => {
            tracing::warn!(target: "regia_editor", "curve table autosave: {err}");
            None
        }
    }
}

fn chooser_snapshot(doc: &super::ChooserDoc) -> DocSnapshot {
    DocSnapshot {
        key: key(&doc.path),
        bytes: doc.table.to_ron_string().into_bytes(),
    }
}

fn data_snapshot(doc: &super::data::DataDoc) -> DocSnapshot {
    DocSnapshot {
        key: key(&doc.path),
        bytes: doc.text().into_bytes(),
    }
}

fn snapshot(world: &World) -> Vec<DocSnapshot> {
    let Some(panel) = world.get_resource::<TablesPanel>() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(doc) = panel.curves.as_ref().filter(|d| d.dirty) {
        out.extend(curve_snapshot(doc));
    }
    if let Some(doc) = panel.chooser.as_ref().filter(|d| d.dirty) {
        out.push(chooser_snapshot(doc));
    }
    if let Some(doc) = panel.data.as_ref().filter(|d| d.dirty) {
        out.push(data_snapshot(doc));
    }
    out
}

/// The retained tables' registry entry (P-322 part b): a table opened before the one its tab
/// shows stays in memory with its unsaved edits (the shared history can bring it back); its
/// newest copy is autosaved while dirty, unless the panel has the same file open again (then the
/// open copy is the one that counts).
pub(crate) const RETAINED: RetainedProvider = RetainedProvider {
    kind: KIND,
    dirty: retained_dirty,
    snapshot: retained_snapshot,
};

fn table_path(doc: &super::TableDocument) -> &str {
    match doc {
        super::TableDocument::Curves(d) => &d.path,
        super::TableDocument::Chooser(d) => &d.path,
        super::TableDocument::Data(d) => &d.path,
    }
}

fn table_dirty(doc: &super::TableDocument) -> bool {
    match doc {
        super::TableDocument::Curves(d) => d.dirty,
        super::TableDocument::Chooser(d) => d.dirty,
        super::TableDocument::Data(d) => d.dirty,
    }
}

/// The newest retained copy of each file the panel does not have open, while dirty.
fn retained_tables(world: &World) -> Vec<&super::TableDocument> {
    let Some(docs) = world.get_resource::<super::TableDocuments>() else {
        return Vec::new();
    };
    let open: Vec<&str> = world
        .get_resource::<TablesPanel>()
        .map_or_else(Vec::new, |panel| {
            let curves = panel.curves.as_ref().map(|d| d.path.as_str());
            let chooser = panel.chooser.as_ref().map(|d| d.path.as_str());
            let data = panel.data.as_ref().map(|d| d.path.as_str());
            [curves, chooser, data].into_iter().flatten().collect()
        });
    // Ids grow with each document the panel opens: the last copy of a path is its newest.
    let mut newest: std::collections::BTreeMap<&str, &super::TableDocument> =
        std::collections::BTreeMap::new();
    for doc in docs.retired.values() {
        newest.insert(table_path(doc), doc);
    }
    newest
        .into_iter()
        .filter(|(path, doc)| !open.contains(path) && table_dirty(doc))
        .map(|(_, doc)| doc)
        .collect()
}

fn retained_dirty(world: &World) -> Vec<DocKey> {
    retained_tables(world)
        .into_iter()
        .map(|doc| key(table_path(doc)))
        .collect()
}

fn retained_snapshot(world: &World) -> Vec<DocSnapshot> {
    retained_tables(world)
        .into_iter()
        .filter_map(|doc| match doc {
            super::TableDocument::Curves(d) => curve_snapshot(d),
            super::TableDocument::Chooser(d) => Some(chooser_snapshot(d)),
            super::TableDocument::Data(d) => Some(data_snapshot(d)),
        })
        .collect()
}

/// The path under `assets/` of an absolute or project-relative original.
fn assets_relative(world: &World, original: &Path) -> String {
    let root: Option<PathBuf> = crate::matgraph_editor::project_root(world);
    let rel = root
        .as_deref()
        .and_then(|root| original.strip_prefix(root).ok())
        .unwrap_or(original);
    let rel = rel.strip_prefix("assets").unwrap_or(rel);
    rel.to_string_lossy().replace('\\', "/")
}

/// Opens the original file on its tab, then puts the recovered table over it, marked dirty.
fn restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let original = key
        .original
        .as_deref()
        .ok_or_else(|| "a table autosave needs its file".to_string())?;
    let path = assets_relative(world, original);
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not text: {e}"))?;
    let project: Option<PathBuf> = crate::matgraph_editor::project_root(world);
    let mut panel = world.get_resource_or_insert_with(TablesPanel::default);
    panel.open(project.as_deref(), &path).map_err(|e| e.message)?;
    let lower = path.to_ascii_lowercase();
    if super::DATA_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
        let mut doc =
            super::data::DataDoc::open(&path, text, &SchemaLookup::none()).map_err(|e| e.message)?;
        doc.dirty = true;
        panel.data = Some(doc);
    } else if super::CHOOSER_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
        let table = regia_framework::chooser::ChooserTable::from_ron_str(text).map_err(|e| e.to_string())?;
        if let Some(doc) = panel.chooser.as_mut() {
            doc.table = table;
            doc.issues = doc.table.validate(&doc.nested);
            doc.dirty = true;
        }
    } else {
        let table =
            regia_framework::curve_table::parse_curve_table(&path, text).map_err(|e| e.to_string())?;
        if let Some(doc) = panel.curves.as_mut() {
            doc.issues = table.validate();
            doc.table = table;
            doc.dirty = true;
        }
    }
    Ok(())
}

/// The Save all hook (P-147, ADR-8147): the tab that holds the key's table is saved, as its Save
/// button saves it.
pub(crate) fn save(world: &mut World, doc: &DocKey) -> Result<(), String> {
    let Some(panel) = world.get_resource::<TablesPanel>() else {
        return Ok(());
    };
    let matches = |path: Option<&String>| path.is_some_and(|p| key(p).original == doc.original);
    let tab = if matches(panel.curves.as_ref().filter(|d| d.dirty).map(|d| &d.path)) {
        super::TablesTab::Curves
    } else if matches(panel.chooser.as_ref().filter(|d| d.dirty).map(|d| &d.path)) {
        super::TablesTab::Chooser
    } else if matches(panel.data.as_ref().filter(|d| d.dirty).map(|d| &d.path)) {
        super::TablesTab::Data
    } else {
        return Ok(());
    };
    super::with_panel(world, |panel, project| panel.save(project, Some(tab)))
        .map(|_| ())
        .map_err(|e| e.message)
}
