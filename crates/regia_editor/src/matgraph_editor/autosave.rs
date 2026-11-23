// SPDX-License-Identifier: MPL-2.0
//! Document autosave of the material graph tabs (`crate::doc_autosave`, ADR-9770): every open
//! graph, function or instance with unsaved work, active or parked.
//!
//! The bytes are a small RON envelope holding exactly what a save writes: the main file's text
//! and, for graphs and functions, the layout sidecar's text, plus an instance's parent file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use regia_matgraph::{MaterialFunction, MaterialGraph, MaterialInstance};
use serde::{Deserialize, Serialize};

use super::{DocMode, GraphLayoutFile, LAYOUT_FORMAT_VERSION, MatGraphDoc, MatGraphEditor};
use crate::doc_autosave::{DocKey, DocProvider, DocSnapshot};

/// The provider kind.
pub(crate) const KIND: &str = "matgraph";

/// The registry entry.
pub(crate) const PROVIDER: DocProvider = DocProvider {
    kind: KIND,
    dirty,
    snapshot,
    restore,
};

/// What one tab's autosave holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Envelope {
    /// `graph`, `function` or `instance`.
    mode: String,
    /// The main file's text.
    text: String,
    /// The layout sidecar's text (graphs and functions).
    layout: Option<String>,
    /// An instance's parent graph file.
    parent: Option<PathBuf>,
}

/// One tab that would be saved by Save and exit.
struct UnsavedTab<'a> {
    path: Option<PathBuf>,
    doc: &'a MatGraphDoc,
    parent: Option<PathBuf>,
    session: u64,
}

/// Every tab that would be saved by Save and exit.
fn unsaved_tabs(editor: &MatGraphEditor) -> Vec<UnsavedTab<'_>> {
    let mut out = Vec::new();
    if editor.is_open() && MatGraphEditor::needs_save(editor.session, &editor.doc, &editor.saved) {
        out.push(UnsavedTab {
            path: editor.path.clone(),
            doc: &editor.doc,
            parent: editor.parent_path.clone(),
            session: editor.session,
        });
    }
    for tab in &editor.parked {
        if MatGraphEditor::needs_save(tab.session, &tab.doc, &tab.saved) {
            out.push(UnsavedTab {
                path: tab.path.clone(),
                doc: &tab.doc,
                parent: tab.parent_path.clone(),
                session: tab.session,
            });
        }
    }
    out
}

/// The key of one tab. An untitled tab is told apart by its session number, which stays with
/// the tab while it is parked, moved or another tab is saved (its position among the unsaved
/// tabs does not), or by the label of the autosave it was restored from. A new tab's label never
/// equals a restored tab's (the two would share one autosave file).
fn key_of(editor: &MatGraphEditor, path: Option<&Path>, doc: &MatGraphDoc, session: u64) -> DocKey {
    let mut key = DocKey::new(KIND, path);
    if path.is_none() {
        key.label = editor.autosave_labels.get(&session).cloned().unwrap_or_else(|| {
            let mut label = format!("untitled material {} {session}", super::doc_kind(doc));
            while editor.autosave_labels.values().any(|restored| *restored == label) {
                label.push_str(" (new)");
            }
            label
        });
    }
    key
}

fn dirty(world: &World) -> Vec<DocKey> {
    let Some(editor) = world.get_resource::<MatGraphEditor>() else {
        return Vec::new();
    };
    unsaved_tabs(editor)
        .into_iter()
        .map(|tab| key_of(editor, tab.path.as_deref(), tab.doc, tab.session))
        .collect()
}

/// The texts a save of `doc` writes.
fn envelope(doc: &MatGraphDoc, parent: Option<PathBuf>) -> Result<Envelope, String> {
    let (mode, text) = match &doc.mode {
        DocMode::Graph => ("graph", doc.graph.to_ron().map_err(|e| e.to_string())?),
        DocMode::Function(meta) => (
            "function",
            meta.to_function(&doc.graph.nodes)
                .to_ron()
                .map_err(|e| e.to_string())?,
        ),
        DocMode::Instance(instance) => ("instance", instance.to_ron().map_err(|e| e.to_string())?),
    };
    let layout = if matches!(doc.mode, DocMode::Instance(_)) {
        None
    } else {
        // The same sidecar a save writes: positions, comment frames and reroutes (layout v2).
        let file = super::layout_file_of(doc);
        Some(ron::ser::to_string_pretty(&file, regia_core::ron_pretty()).map_err(|e| e.to_string())?)
    };
    Ok(Envelope {
        mode: mode.to_string(),
        text,
        layout,
        parent,
    })
}

fn snapshot(world: &World) -> Vec<DocSnapshot> {
    let Some(editor) = world.get_resource::<MatGraphEditor>() else {
        return Vec::new();
    };
    unsaved_tabs(editor)
        .into_iter()
        .filter_map(|tab| {
            let key = key_of(editor, tab.path.as_deref(), tab.doc, tab.session);
            let encoded = envelope(tab.doc, tab.parent).and_then(|env| {
                ron::ser::to_string_pretty(&env, regia_core::ron_pretty()).map_err(|e| e.to_string())
            });
            match encoded {
                Ok(text) => Some(DocSnapshot {
                    key,
                    bytes: text.into_bytes(),
                }),
                Err(err) => {
                    tracing::warn!(target: "regia_editor", "material graph autosave of {}: {err}", key.label);
                    None
                }
            }
        })
        .collect()
}

fn layout_of(env: &Envelope) -> GraphLayoutFile {
    env.layout
        .as_deref()
        .and_then(|text| ron::from_str::<GraphLayoutFile>(text).ok())
        .filter(|file| file.format_version <= LAYOUT_FORMAT_VERSION)
        .unwrap_or_default()
}

/// Opens the envelope in a new tab at the original path, never saved (dirty until a save).
fn restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not text: {e}"))?;
    let env: Envelope = ron::from_str(text).map_err(|e| format!("cannot read the autosave: {e}"))?;
    let doc = match env.mode.as_str() {
        "graph" => super::with_layout(
            MatGraphDoc::new(
                MaterialGraph::from_ron(&env.text).map_err(|e| e.to_string())?,
                BTreeMap::new(),
            ),
            layout_of(&env),
        ),
        "function" => super::with_layout(
            MatGraphDoc::function(
                MaterialFunction::from_ron(&env.text).map_err(|e| e.to_string())?,
                BTreeMap::new(),
            ),
            layout_of(&env),
        ),
        "instance" => {
            let parent = env
                .parent
                .clone()
                .ok_or_else(|| "the instance autosave names no parent graph".to_string())?;
            let parent_text = std::fs::read_to_string(&parent)
                .map_err(|e| format!("cannot read the parent {}: {e}", parent.display()))?;
            let graph = MaterialGraph::from_ron(&parent_text).map_err(|e| e.to_string())?;
            let instance = MaterialInstance::from_ron(&env.text).map_err(|e| e.to_string())?;
            super::with_layout(
                MatGraphDoc::instance(graph, BTreeMap::new(), instance),
                super::read_layout_file(&parent),
            )
        }
        other => return Err(format!("unknown material document mode `{other}`")),
    };
    // The file on disk is the saved copy, so the editor shows the recovered edits as unsaved
    // (an untitled or vanished file: never saved).
    let on_disk = key.original.as_deref().and_then(|path| {
        let text = std::fs::read_to_string(path).ok()?;
        match env.mode.as_str() {
            "graph" => MaterialGraph::from_ron(&text).ok().map(|g| {
                super::with_layout(
                    MatGraphDoc::new(g, BTreeMap::new()),
                    super::read_layout_file(path),
                )
            }),
            "function" => MaterialFunction::from_ron(&text).ok().map(|f| {
                super::with_layout(
                    MatGraphDoc::function(f, BTreeMap::new()),
                    super::read_layout_file(path),
                )
            }),
            _ => None,
        }
    });
    let mut editor = world.get_resource_or_insert_with(MatGraphEditor::default);
    editor.open_tab();
    editor.reset_document(doc, key.original.clone(), false);
    editor.saved = on_disk;
    editor.parent_path = env.parent;
    if key.original.is_none() {
        // The restored tab keeps the id of its autosave: the next tick overwrites that file.
        let session = editor.session;
        editor.autosave_labels.insert(session, key.label.clone());
    }
    editor.toast = format!("recovered {}", key.label);
    Ok(())
}

/// The Save all hook (P-147, ADR-8147): the tab whose file the key names is saved (the active tab
/// is put back after); an untitled tab is saved from the editor.
pub(crate) fn save(world: &mut World, key: &DocKey) -> Result<(), String> {
    let Some(original) = key.original.clone() else {
        return Err("untitled: save it from the material graph editor".to_string());
    };
    let Some(mut editor) = world.get_resource_mut::<MatGraphEditor>() else {
        return Ok(());
    };
    if editor.path.as_deref() == Some(original.as_path()) {
        return editor.save(None).map(|_| ());
    }
    let restore = editor.active_tab;
    let count = editor.tabs().len();
    let mut saved = Ok(());
    for index in 0..count {
        editor.select_tab(index)?;
        if editor.path.as_deref() == Some(original.as_path()) {
            if MatGraphEditor::needs_save(editor.session, &editor.doc, &editor.saved) {
                saved = editor.save(None).map(|_| ());
            }
            break;
        }
    }
    if count > 0 && restore < count && editor.active_tab != restore {
        return saved.and(editor.select_tab(restore));
    }
    saved
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two untitled graphs with edits: the active one and a parked one.
    fn two_untitled_tabs() -> World {
        let mut editor = MatGraphEditor::default();
        editor.new_graph(None);
        editor
            .doc
            .add_node(Some("c"), regia_matgraph::Node::Float(0.5), None)
            .expect("add");
        editor.open_tab();
        editor.new_graph(None);
        editor
            .doc
            .add_node(Some("d"), regia_matgraph::Node::Float(1.0), None)
            .expect("add");
        let mut world = World::new();
        world.insert_resource(editor);
        world
    }

    /// Review sprint-rev-claude-50-k, autosave-recovery finding 5: an untitled tab's autosave id
    /// came from its position among the unsaved tabs, so saving one untitled tab renumbered the
    /// other (its autosave was removed and written again under a new id). The id stays.
    #[test]
    fn untitled_tab_autosave_id_survives_saving_another_tab() {
        let dir = regia_test_utils::unique_temp_dir("matgraph-autosave-ids");
        std::fs::create_dir_all(&dir).expect("dir");
        let mut world = two_untitled_tabs();
        let before = dirty(&world);
        assert_eq!(before.len(), 2, "{before:?}");
        assert_ne!(before[0].id(), before[1].id());
        // Save the active tab: the parked one is the only unsaved tab left.
        world
            .resource_mut::<MatGraphEditor>()
            .save(Some(&dir.join("saved.matgraph.ron")))
            .expect("save");
        let after = dirty(&world);
        assert_eq!(after.len(), 1, "{after:?}");
        assert_eq!(
            after[0].id(),
            before[1].id(),
            "the survivor keeps its autosave id"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Integration (ADR-7381): the autosave carries the layout v2 sidecar a save writes, so a
    /// recovered graph keeps its comment frames and reroutes, not only its node positions.
    #[test]
    fn restored_graph_keeps_its_comment_frames_and_reroutes() {
        let mut world = two_untitled_tabs();
        {
            let mut editor = world.resource_mut::<MatGraphEditor>();
            editor.doc.notes.comments.push(super::super::model::CommentFrame {
                text: "albedo".into(),
                min: [0.0, 0.0],
                max: [120.0, 80.0],
                color: [40, 60, 80],
            });
        }
        let active = dirty(&world)[0].clone();
        let snapshots = snapshot(&world);
        let saved = snapshots
            .iter()
            .find(|s| s.key == active)
            .expect("active snapshot");
        let mut fresh = World::new();
        restore(&mut fresh, &saved.key, &saved.bytes).expect("restore");
        let editor = fresh.resource::<MatGraphEditor>();
        assert_eq!(editor.doc.notes.comments.len(), 1, "{:?}", editor.doc.notes);
        assert_eq!(editor.doc.notes.comments[0].text, "albedo");
    }

    /// A restored untitled tab keeps the id of its autosave (the next tick overwrites that file
    /// instead of removing it and writing a new one).
    #[test]
    fn restored_untitled_tab_keeps_its_autosave_id() {
        let world = two_untitled_tabs();
        let snapshots = snapshot(&world);
        let parked = snapshots
            .iter()
            .find(|s| s.key == dirty(&world)[1])
            .expect("parked snapshot");
        let mut fresh = World::new();
        restore(&mut fresh, &parked.key, &parked.bytes).expect("restore");
        let keys = dirty(&fresh);
        assert_eq!(keys.len(), 1, "{keys:?}");
        assert_eq!(keys[0].id(), parked.key.id());
    }

    /// A new untitled tab whose session number gives the label of a restored tab gets another
    /// label: the two never share one autosave file.
    #[test]
    fn new_untitled_tab_never_shares_a_restored_tabs_id() {
        let world = two_untitled_tabs();
        let snapshots = snapshot(&world);
        // This session: a new graph first (session 1), then the restore of the crashed
        // session's first untitled tab (whose label also says 1).
        let mut fresh = World::new();
        {
            let mut editor = fresh.get_resource_or_insert_with(MatGraphEditor::default);
            editor.new_graph(None);
            editor
                .doc
                .add_node(Some("e"), regia_matgraph::Node::Float(2.0), None)
                .expect("add");
        }
        let restored = snapshots
            .iter()
            .find(|s| s.key.label.ends_with(" 1"))
            .expect("a label ending in 1");
        restore(&mut fresh, &restored.key, &restored.bytes).expect("restore");
        let keys = dirty(&fresh);
        assert_eq!(keys.len(), 2, "{keys:?}");
        assert_ne!(keys[0].id(), keys[1].id(), "{keys:?}");
        assert!(keys.iter().any(|k| k.id() == restored.key.id()));
    }
}
