// SPDX-License-Identifier: MPL-2.0
//! Document autosave of the procedural graph editor (`crate::doc_autosave`, ADR-9770): the open
//! graph or variant, as its save writes it.

use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use regia_procgen::ProcGraph;
use regia_procgen::graph::ProcGraphVariant;

use super::doc::{self, ProcGraphDoc};
use crate::doc_autosave::{DocKey, DocProvider, DocSnapshot};

/// The provider kind.
pub(crate) const KIND: &str = "procgen";

/// The registry entry.
pub(crate) const PROVIDER: DocProvider = DocProvider {
    kind: KIND,
    dirty,
    snapshot,
    restore,
};

/// The file the save writes (the variant's when one is open) and its text.
fn save_target(doc: &ProcGraphDoc) -> Option<(String, String)> {
    let path = doc.path.clone()?;
    Some(match &doc.variant {
        Some(v) => (v.path.clone(), v.variant.to_ron()),
        None => (path, doc.graph.to_ron()),
    })
}

fn dirty(world: &World) -> Vec<DocKey> {
    world
        .get_resource::<ProcGraphDoc>()
        .filter(|d| d.dirty())
        .and_then(save_target)
        .map(|(file, _)| vec![DocKey::new(KIND, Some(Path::new(&file)))])
        .unwrap_or_default()
}

fn snapshot(world: &World) -> Vec<DocSnapshot> {
    world
        .get_resource::<ProcGraphDoc>()
        .filter(|d| d.dirty())
        .and_then(save_target)
        .map(|(file, text)| {
            vec![DocSnapshot {
                key: DocKey::new(KIND, Some(Path::new(&file))),
                bytes: text.into_bytes(),
            }]
        })
        .unwrap_or_default()
}

/// The project-relative form the procedural graph editor addresses files by.
fn relative(world: &World, path: &Path) -> String {
    let root: Option<PathBuf> = crate::matgraph_editor::project_root(world);
    root.as_deref()
        .and_then(|root| path.strip_prefix(root).ok())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Opens the original file, then puts the recovered graph (or variant) over it: dirty, because
/// the editor's disk text is the file's.
fn restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let original = key
        .original
        .as_deref()
        .ok_or_else(|| "a procedural graph autosave needs its file".to_string())?;
    let rel = relative(world, original);
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not text: {e}"))?;
    // Parse first: a bad autosave leaves the open document alone.
    let recovered = if rel.ends_with(".pgvar.ron") {
        Err(ProcGraphVariant::from_ron(text).map_err(|e| e.to_string())?)
    } else {
        Ok(ProcGraph::from_ron(text).map_err(|e| e.to_string())?)
    };
    doc::open(world, &rel).map_err(|e| e.to_string())?;
    {
        let mut doc = world.resource_mut::<ProcGraphDoc>();
        match recovered {
            Ok(graph) => doc.graph = graph,
            Err(variant) => {
                if let Some(view) = doc.variant.as_mut() {
                    view.variant = variant;
                }
            }
        }
        doc.revision += 1;
        doc.toast = format!("recovered {rel}");
    }
    doc::changed(world);
    Ok(())
}

/// The Save all hook (P-147, ADR-8147): the procedural graph editor's Save.
pub(crate) fn save(world: &mut World, _key: &DocKey) -> Result<(), String> {
    if dirty(world).is_empty() {
        return Ok(());
    }
    doc::save(world)
        .map(|_| ())
        .map_err(|e| format!("{} ({})", e.message, e.code))
}
