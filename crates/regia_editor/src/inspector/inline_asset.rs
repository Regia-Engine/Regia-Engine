// SPDX-License-Identifier: MPL-2.0
//! Inline sub-asset editing in the Inspector (spec addendum § 7.11, ADR-8107): the expand arrow of
//! an asset field shows the referenced asset's fields under the field, and edits them in place.
//!
//! **Which assets.** The kinds with a typed document the editor can read, check and write back
//! canonically: data assets (`.data.ron`, the value read through the type registry, the same
//! document the Data Asset panel edits), standard materials (`.material.ron`,
//! `regia_asset::MaterialAssetRon`) and physics materials (`.physmat.ron`,
//! `regia_physics::PhysicsMaterial`). Every other kind (material graphs and instances, meshes,
//! textures, curve tables, levels, prefabs) keeps only the picker: its own editor edits it.
//!
//! **The document model.** One [`InlineDoc`] per asset file, shared by every field that names it.
//! The fields shown are the document's JSON view, the same view `editor.edit_asset` patches (dotted
//! paths, numeric segments index arrays). An edit sets one leaf, and the whole document goes back
//! through the kind's typed reader before it is taken: a value the type or its validation refuses
//! is refused with the reason shown, and the document is unchanged.
//!
//! **Where edits go.** To the referenced file, never a hidden copy: each edit is one undo step on
//! the editor's history (a drag is one step) that changes the document in memory and leaves the
//! level clean; the file is written by **Save** in the inline header or by File ▸ Save all, which
//! lists every inline document with unsaved edits (this module's `crate::doc_autosave` provider,
//! kind [`KIND`]). Autosave keeps their bytes like any asset editor's.
//!
//! **Make unique** stays the picker's (`super::make_unique`): it copies the file next to the level
//! and repoints the field in one undo step; the copy opens as its own inline document. Unsaved
//! inline edits stay with the original file's document (Save all lists it).
//!
//! The agent surface is `editor.inspect_asset_inline` and `editor.asset_inline_set`; the file is
//! saved through `editor.save_all` with the `save_id` they answer (`book/agents/inspector.md`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, TypeRegistry};
use serde_json::{Value, json};

use super::picker_links::{PickerLinks, PickerRequest};
use crate::command::{EditorCommand, UndoStack};
use crate::data_asset::DataAssetDoc;
use crate::data_panels::PanelError;
use crate::doc_autosave::{DocAutosaveRegistry, DocKey, DocProvider, DocSnapshot};
use crate::viewport_agent::Handler;

/// The document autosave and Save all kind of inline asset documents.
pub const KIND: &str = "inline_asset";

const HINT: &str = "see book/agents/inspector.md (inline asset editing)";

/// The asset kinds the Inspector edits inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineKind {
    /// A data asset (`.data.ron`): one reflected value.
    DataAsset,
    /// A standard material (`.material.ron`).
    Material,
    /// A physics material (`.physmat.ron`).
    PhysicsMaterial,
}

impl InlineKind {
    /// The kind of the asset at `path` (by its compound extension), when it is edited inline.
    #[must_use]
    pub fn of_path(path: &str) -> Option<Self> {
        let lower = path.trim().to_ascii_lowercase();
        if lower.ends_with(".data.ron") {
            Some(Self::DataAsset)
        } else if lower.ends_with(".material.ron") {
            Some(Self::Material)
        } else if lower.ends_with(".physmat.ron") {
            Some(Self::PhysicsMaterial)
        } else {
            None
        }
    }

    /// The agent's name of the kind.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::DataAsset => "data_asset",
            Self::Material => "material",
            Self::PhysicsMaterial => "physics_material",
        }
    }
}

/// What a field draw sees of one inline document.
#[derive(Debug, Clone, PartialEq)]
pub struct InlineView {
    /// The document's kind (`None` when it could not be opened).
    pub kind: Option<InlineKind>,
    /// The JSON view of the document (`null` when it could not be opened).
    pub value: Value,
    /// Whether the document has unsaved edits.
    pub dirty: bool,
    /// Why the document could not be opened, or why the last edit or save was refused.
    pub error: Option<String>,
}

/// The inline documents by project-relative path, as a field draw sees them.
pub type InlineViews = Arc<BTreeMap<String, Arc<InlineView>>>;

/// One open inline document.
#[derive(Debug)]
pub struct InlineDoc {
    /// The kind.
    pub kind: InlineKind,
    /// The JSON view (canonical: what the typed reader gives back).
    value: Value,
    /// The text a save writes now.
    text: String,
    /// The text last read or saved: the document is dirty while `text` differs.
    saved_text: String,
    /// A data asset's reflected document.
    data: Option<DataAssetDoc>,
}

impl InlineDoc {
    /// Whether the document differs from its file.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.text != self.saved_text
    }

    /// The JSON view.
    #[must_use]
    pub fn value(&self) -> &Value {
        &self.value
    }

    /// The text a save writes.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Every inline document of the session (an edit-world resource).
#[derive(Resource, Debug, Default)]
pub struct InlineAssetDocs {
    docs: BTreeMap<String, InlineDoc>,
    /// The last refusal per path (an open, an edit or a save), shown in the inline header.
    errors: BTreeMap<String, String>,
    views: InlineViews,
}

impl InlineAssetDocs {
    /// The document at `path` (project-relative, `assets/...`).
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&InlineDoc> {
        self.docs.get(path)
    }

    /// The paths of the documents with unsaved edits.
    #[must_use]
    pub fn dirty_paths(&self) -> Vec<String> {
        self.docs
            .iter()
            .filter(|(_, doc)| doc.dirty())
            .map(|(path, _)| path.clone())
            .collect()
    }

    /// The draw snapshot (shared, rebuilt only when a document changes).
    #[must_use]
    pub fn views(&self) -> InlineViews {
        Arc::clone(&self.views)
    }

    /// Rebuilds the view of `path` after a change.
    fn refresh_view(&mut self, path: &str) {
        let mut map = (*self.views).clone();
        let error = self.errors.get(path).cloned();
        match self.docs.get(path) {
            Some(doc) => {
                map.insert(
                    path.to_owned(),
                    Arc::new(InlineView {
                        kind: Some(doc.kind),
                        value: doc.value.clone(),
                        dirty: doc.dirty(),
                        error,
                    }),
                );
            }
            None if error.is_some() => {
                map.insert(
                    path.to_owned(),
                    Arc::new(InlineView {
                        kind: None,
                        value: Value::Null,
                        dirty: false,
                        error,
                    }),
                );
            }
            None => {
                map.remove(path);
            }
        }
        self.views = Arc::new(map);
    }

    fn set_error(&mut self, path: &str, error: Option<String>) {
        match error {
            Some(error) => self.errors.insert(path.to_owned(), error),
            None => self.errors.remove(path),
        };
        self.refresh_view(path);
    }
}

/// The draw snapshot of the session's inline documents (empty when none is open).
#[must_use]
pub fn views(world: &World) -> InlineViews {
    world
        .get_resource::<InlineAssetDocs>()
        .map(InlineAssetDocs::views)
        .unwrap_or_default()
}

fn err(code: &str, message: impl Into<String>) -> PanelError {
    PanelError::new(code, message)
}

/// `path` as the project-relative `assets/...` form, refusing anything that could leave the
/// project.
///
/// # Errors
///
/// `asset_inline_bad_path`.
pub fn normalize_path(path: &str) -> Result<String, PanelError> {
    let bad = || {
        err(
            "asset_inline_bad_path",
            format!("{path:?} is not a project path under assets/"),
        )
    };
    let rel = super::impls::asset_rel_path(path).ok_or_else(bad)?;
    if rel.contains(':')
        || rel.contains('\\')
        || rel
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(bad());
    }
    Ok(rel)
}

fn project_root(world: &World) -> Result<PathBuf, PanelError> {
    crate::mesh_source::project_root(world)
        .ok_or_else(|| err("asset_inline_no_project", "no project is open"))
}

fn registry_of(world: &World) -> Option<AppTypeRegistry> {
    world.get_resource::<AppTypeRegistry>().cloned()
}

/// The Save all / autosave key of the document at `rel`.
#[must_use]
pub fn doc_key(root: &Path, rel: &str) -> DocKey {
    DocKey::new(KIND, Some(&root.join(rel)))
}

/// Reads the document at `rel` from disk.
fn load(
    root: &Path,
    rel: &str,
    kind: InlineKind,
    registry: Option<&TypeRegistry>,
) -> Result<InlineDoc, PanelError> {
    let full = root.join(rel);
    if kind == InlineKind::DataAsset {
        let registry =
            registry.ok_or_else(|| err("asset_inline_parse", "the type registry is not available"))?;
        let data = DataAssetDoc::open(root, rel, registry).map_err(|e| {
            let code = if e.code == "data_asset_not_found" {
                "asset_inline_not_found"
            } else {
                "asset_inline_parse"
            };
            err(code, e.message)
        })?;
        let text = data
            .to_text(registry)
            .map_err(|e| err("asset_inline_parse", e.message))?;
        return Ok(InlineDoc {
            kind,
            value: data.value_json(registry),
            saved_text: text.clone(),
            text,
            data: Some(data),
        });
    }
    let text = std::fs::read_to_string(&full)
        .map_err(|e| err("asset_inline_not_found", format!("cannot read {rel}: {e}")))?;
    let (value, canonical) = decode_text(kind, &text)?;
    Ok(InlineDoc {
        kind,
        value,
        saved_text: canonical.clone(),
        text: canonical,
        data: None,
    })
}

/// A material or physics material's JSON view and canonical text from file text.
fn decode_text(kind: InlineKind, text: &str) -> Result<(Value, String), PanelError> {
    let parse = |e: String| err("asset_inline_parse", e);
    match kind {
        InlineKind::Material => {
            let material =
                regia_asset::MaterialAssetRon::from_ron(text.as_bytes()).map_err(|e| parse(e.to_string()))?;
            canonical_material(&material)
        }
        InlineKind::PhysicsMaterial => {
            let material =
                regia_physics::PhysicsMaterial::from_ron(text).map_err(|e| parse(e.to_string()))?;
            canonical_physmat(&material)
        }
        InlineKind::DataAsset => Err(parse("a data asset is read through its file".to_owned())),
    }
}

fn canonical_material(material: &regia_asset::MaterialAssetRon) -> Result<(Value, String), PanelError> {
    let text = material
        .to_ron_string()
        .map_err(|e| err("asset_inline_bad_value", e.to_string()))?;
    let value = serde_json::to_value(material).map_err(|e| err("asset_inline_bad_value", e.to_string()))?;
    Ok((value, text))
}

fn canonical_physmat(material: &regia_physics::PhysicsMaterial) -> Result<(Value, String), PanelError> {
    material
        .validate()
        .map_err(|e| err("asset_inline_bad_value", e.to_string()))?;
    let text = material
        .to_ron()
        .map_err(|e| err("asset_inline_bad_value", e.to_string()))?;
    let value = serde_json::to_value(material).map_err(|e| err("asset_inline_bad_value", e.to_string()))?;
    Ok((value, text))
}

/// Puts the whole JSON view `value` into `doc` through its typed reader. The document is unchanged
/// on error.
fn take_value(doc: &mut InlineDoc, value: &Value, registry: Option<&TypeRegistry>) -> Result<(), PanelError> {
    let bad = |e: String| err("asset_inline_bad_value", e);
    let (canonical, text) = match doc.kind {
        InlineKind::Material => {
            let material: regia_asset::MaterialAssetRon = serde_json::from_value(value.clone())
                .map_err(|e| bad(format!("does not fit the material: {e}")))?;
            canonical_material(&material)?
        }
        InlineKind::PhysicsMaterial => {
            let material: regia_physics::PhysicsMaterial = serde_json::from_value(value.clone())
                .map_err(|e| bad(format!("does not fit the physics material: {e}")))?;
            canonical_physmat(&material)?
        }
        InlineKind::DataAsset => {
            let registry = registry.ok_or_else(|| bad("the type registry is not available".to_owned()))?;
            let data = doc
                .data
                .as_mut()
                .ok_or_else(|| bad("the data asset is not open".to_owned()))?;
            data.set_field("", value, registry).map_err(|e| bad(e.message))?;
            let text = data.to_text(registry).map_err(|e| bad(e.message))?;
            (data.value_json(registry), text)
        }
    };
    doc.value = canonical;
    doc.text = text;
    Ok(())
}

/// Sets the leaf at `field` (dot-separated; numeric segments index arrays; empty for the whole
/// value) of a JSON view.
///
/// # Errors
///
/// `asset_inline_no_field` when a segment names nothing.
pub fn set_at(view: &mut Value, field: &str, value: Value) -> Result<(), PanelError> {
    if field.is_empty() {
        *view = value;
        return Ok(());
    }
    let mut current = view;
    for segment in field.split('.') {
        let missing = || {
            err(
                "asset_inline_no_field",
                format!("`{field}` has no field `{segment}`"),
            )
        };
        current = match current {
            Value::Object(map) => map.get_mut(segment).ok_or_else(missing)?,
            Value::Array(items) => {
                let index: usize = segment.parse().map_err(|_| missing())?;
                items.get_mut(index).ok_or_else(missing)?
            }
            _ => return Err(missing()),
        };
    }
    *current = value;
    Ok(())
}

/// Registers the Save all / autosave provider and its save hook (idempotent).
///
/// Opening an inline document from a world that did not run the editor's full provider
/// install (the inline tests, and any session that reaches the inspector first) still has
/// to be savable from File ▸ Save all. The hook is what marks the row `can_save`.
fn ensure_provider(world: &mut World) {
    let mut registry = world.get_resource_or_insert_with(DocAutosaveRegistry::default);
    if registry.provider(KIND).is_none() {
        registry.register(PROVIDER);
    }
    if registry.save_hook(KIND).is_none() {
        registry.register_save(KIND, save_hook, false);
    }
}

/// Opens the inline document of `path` (or re-reads it from disk when it has no unsaved edits, so
/// a change made elsewhere shows). Returns the project-relative path.
///
/// # Errors
///
/// `asset_inline_bad_path`, `asset_inline_unsupported`, `asset_inline_no_project`,
/// `asset_inline_not_found`, `asset_inline_parse`.
pub fn open(world: &mut World, path: &str) -> Result<String, PanelError> {
    let rel = normalize_path(path)?;
    let result = open_rel(world, &rel);
    let mut docs = world.get_resource_or_insert_with(InlineAssetDocs::default);
    docs.set_error(&rel, result.as_ref().err().map(|e| e.message.clone()));
    result.map(|()| rel)
}

fn open_rel(world: &mut World, rel: &str) -> Result<(), PanelError> {
    let kind = InlineKind::of_path(rel).ok_or_else(|| {
        err(
            "asset_inline_unsupported",
            format!("{rel} is not edited inline: data assets, materials and physics materials are"),
        )
    })?;
    let root = project_root(world)?;
    ensure_provider(world);
    let keep = world
        .get_resource::<InlineAssetDocs>()
        .and_then(|docs| docs.get(rel))
        .is_some_and(InlineDoc::dirty);
    if keep {
        return Ok(());
    }
    let registry = registry_of(world);
    let doc = {
        let guard = registry.as_ref().map(|r| r.read());
        load(&root, rel, kind, guard.as_deref())?
    };
    let mut docs = world.get_resource_or_insert_with(InlineAssetDocs::default);
    docs.docs.insert(rel.to_owned(), doc);
    Ok(())
}

/// Whether the asset's own editor holds unsaved edits of `rel` (the inline edit would be lost on
/// its next save).
fn panel_has_unsaved(world: &World, root: &Path, rel: &str, kind: InlineKind) -> bool {
    match kind {
        InlineKind::Material => world
            .get_resource::<crate::material_editor::MaterialPanelStatus>()
            .is_some_and(|status| {
                status.dirty
                    && status
                        .path
                        .as_deref()
                        .is_some_and(|open| crate::material_editor::same_material_file(open, &root.join(rel)))
            }),
        InlineKind::DataAsset => {
            let Some(registry) = registry_of(world) else {
                return false;
            };
            let registry = registry.read();
            world
                .get_resource::<crate::data_asset::DataAssetEditor>()
                .and_then(|editor| editor.doc.as_ref())
                .is_some_and(|doc| doc.path == rel && doc.dirty(&registry))
        }
        InlineKind::PhysicsMaterial => false,
    }
}

/// What [`set_field`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct InlineSet {
    /// The project-relative path.
    pub path: String,
    /// Whether the document changed.
    pub changed: bool,
    /// The value at `field` before the edit.
    pub before: Value,
    /// The value at `field` after the edit (canonical).
    pub after: Value,
    /// Whether the document has unsaved edits now.
    pub dirty: bool,
    /// Undo entries after the edit.
    pub undoable: usize,
}

fn value_at<'a>(view: &'a Value, field: &str) -> Option<&'a Value> {
    if field.is_empty() {
        return Some(view);
    }
    field.split('.').try_fold(view, |current, segment| match current {
        Value::Object(map) => map.get(segment),
        Value::Array(items) => segment.parse::<usize>().ok().and_then(|i| items.get(i)),
        _ => None,
    })
}

/// Sets `field` of the inline document of `path` (opened when needed) as one undo step; a
/// `coalesce` edit (a drag) merges into the previous step of the same field until the history is
/// sealed. The file is written by a save, not here.
///
/// # Errors
///
/// The [`open`] codes, `asset_inline_no_field`, `asset_inline_bad_value` (the typed reader
/// refused the edited document) and `asset_inline_panel_unsaved` (the asset's own editor holds
/// unsaved edits of the file).
pub fn set_field(
    world: &mut World,
    path: &str,
    field: &str,
    value: Value,
    coalesce: bool,
) -> Result<InlineSet, PanelError> {
    let rel = normalize_path(path)?;
    let opened = world
        .get_resource::<InlineAssetDocs>()
        .is_some_and(|docs| docs.get(&rel).is_some());
    if !opened {
        open(world, &rel)?;
    }
    let result = set_open(world, &rel, field, value, coalesce);
    let mut docs = world.get_resource_or_insert_with(InlineAssetDocs::default);
    docs.set_error(&rel, result.as_ref().err().map(|e| e.message.clone()));
    result
}

/// The document views before and after an edit, the field's value before and after, and whether
/// the document is dirty now.
type EditOutcome = (Value, Value, Value, Value, bool);

/// Applies one field edit to the open document `rel` of `docs` (unchanged on error).
fn edit_doc(
    world: &World,
    docs: &mut InlineAssetDocs,
    registry: Option<&TypeRegistry>,
    rel: &str,
    field: &str,
    value: Value,
) -> Result<EditOutcome, PanelError> {
    let root = project_root(world)?;
    let doc = docs
        .docs
        .get_mut(rel)
        .ok_or_else(|| err("asset_inline_not_found", format!("{rel} is not open")))?;
    if panel_has_unsaved(world, &root, rel, doc.kind) {
        return Err(err(
            "asset_inline_panel_unsaved",
            format!("{rel} has unsaved edits in its editor panel: save or undo them there first"),
        ));
    }
    let before_doc = doc.value.clone();
    let before = value_at(&before_doc, field)
        .cloned()
        .ok_or_else(|| err("asset_inline_no_field", format!("`{rel}` has no field `{field}`")))?;
    let mut edited = before_doc.clone();
    set_at(&mut edited, field, value)?;
    take_value(doc, &edited, registry)?;
    let after_doc = doc.value.clone();
    let after = value_at(&after_doc, field).cloned().unwrap_or(Value::Null);
    let dirty = doc.dirty();
    Ok((before_doc, after_doc, before, after, dirty))
}

fn set_open(
    world: &mut World,
    rel: &str,
    field: &str,
    value: Value,
    coalesce: bool,
) -> Result<InlineSet, PanelError> {
    let registry = registry_of(world);
    let mut docs = world.remove_resource::<InlineAssetDocs>().unwrap_or_default();
    let outcome = {
        let guard = registry.as_ref().map(|r| r.read());
        edit_doc(world, &mut docs, guard.as_deref(), rel, field, value)
    };
    docs.refresh_view(rel);
    world.insert_resource(docs);
    let (before_doc, after_doc, before, after, dirty) = outcome?;
    let changed = before_doc != after_doc;
    let undoable = if changed {
        push_edit(
            world,
            InlineAssetEdit {
                path: rel.to_owned(),
                field: field.to_owned(),
                before: before_doc,
                after: after_doc,
                coalesce,
            },
        )
    } else {
        world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth)
    };
    Ok(InlineSet {
        path: rel.to_owned(),
        changed,
        before,
        after,
        dirty,
        undoable,
    })
}

/// Writes the inline document of `path` to its file (atomic). Returns the bytes written.
///
/// # Errors
///
/// `asset_inline_bad_path`, `asset_inline_not_found` (not open), `asset_inline_no_project`,
/// `asset_inline_io`.
pub fn save(world: &mut World, path: &str) -> Result<usize, PanelError> {
    let rel = normalize_path(path)?;
    let root = project_root(world)?;
    let full = root.join(&rel);
    let text = world
        .get_resource::<InlineAssetDocs>()
        .and_then(|docs| docs.get(&rel))
        .map(|doc| doc.text.clone())
        .ok_or_else(|| err("asset_inline_not_found", format!("{rel} is not open inline")))?;
    let written = crate::atomic_write::write_atomic(&full, text.as_bytes())
        .map_err(|e| err("asset_inline_io", format!("cannot write {rel}: {e}")));
    let mut docs = world.get_resource_or_insert_with(InlineAssetDocs::default);
    match &written {
        Ok(()) => {
            if let Some(doc) = docs.docs.get_mut(&rel) {
                doc.saved_text.clone_from(&text);
            }
            docs.set_error(&rel, None);
        }
        Err(e) => docs.set_error(&rel, Some(e.message.clone())),
    }
    written?;
    crate::content_index::note_changed(world, std::slice::from_ref(&full));
    // The Material panel with this file open and clean adopts the saved material.
    let panel_has_it = world
        .get_resource::<crate::material_editor::MaterialPanelStatus>()
        .is_some_and(|status| {
            !status.dirty
                && status
                    .path
                    .as_deref()
                    .is_some_and(|open| crate::material_editor::same_material_file(open, &full))
        });
    if panel_has_it && let Ok(material) = regia_asset::MaterialAssetRon::from_ron(text.as_bytes()) {
        world.insert_resource(crate::material_editor::MaterialPanelReload { path: full, material });
    }
    Ok(text.len())
}

/// The Save all hook (P-147, ADR-8147): writes the inline document the key names.
///
/// # Errors
///
/// The key has no file, the path is outside the project, or [`save`] failed.
pub fn save_hook(world: &mut World, key: &DocKey) -> Result<(), String> {
    let root = project_root(world).map_err(|e| e.message)?;
    let original = key
        .original
        .as_deref()
        .ok_or_else(|| "an inline asset always has a file".to_owned())?;
    let rel = original
        .strip_prefix(&root)
        .map(crate::content::slash_path)
        .map_err(|_| format!("{} is outside the project", original.display()))?;
    save(world, &rel)
        .map(|_| ())
        .map_err(|e| format!("{} ({})", e.message, e.code))
}

// ---------------------------------------------------------------------------
// Undo
// ---------------------------------------------------------------------------

/// One inline edit on the editor's history: the document's JSON view before and after. It changes
/// the asset document, never the level (the level stays clean).
struct InlineAssetEdit {
    path: String,
    field: String,
    before: Value,
    after: Value,
    coalesce: bool,
}

impl InlineAssetEdit {
    fn put(world: &mut World, path: &str, value: &Value) {
        let registry = registry_of(world);
        let guard = registry.as_ref().map(|r| r.read());
        let Some(mut docs) = world.remove_resource::<InlineAssetDocs>() else {
            return;
        };
        if let Some(doc) = docs.docs.get_mut(path)
            && let Err(e) = take_value(doc, value, guard.as_deref())
        {
            tracing::warn!(target: "regia_editor", "inline asset undo on {path}: {}", e.message);
        }
        drop(guard);
        docs.errors.remove(path);
        docs.refresh_view(path);
        world.insert_resource(docs);
    }
}

impl EditorCommand for InlineAssetEdit {
    fn apply(&mut self, world: &mut World) {
        Self::put(world, &self.path, &self.after);
    }
    fn revert(&mut self, world: &mut World) {
        Self::put(world, &self.path, &self.before);
    }
    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = crate::command::downcast_to::<InlineAssetEdit>(next) else {
            return false;
        };
        if self.coalesce && next.coalesce && self.path == next.path && self.field == next.field {
            self.after = next.after.clone();
            true
        } else {
            false
        }
    }
    fn changes_level_state(&self) -> bool {
        // The asset document changes; the level does not (Save all lists the asset).
        false
    }
    fn touched(&self) -> Vec<crate::command::CommandTouch> {
        vec![crate::command::CommandTouch::asset(
            self.path.clone(),
            (!self.field.is_empty()).then(|| self.field.clone()),
        )]
    }
    fn description(&self) -> String {
        if self.field.is_empty() {
            format!("Edit {}", self.path)
        } else {
            format!("Edit {}.{}", self.path, self.field)
        }
    }
    fn activity_targets(&self) -> (Vec<u64>, Vec<String>) {
        (Vec::new(), vec![self.path.clone()])
    }
}

/// Pushes one inline edit: a discrete edit is its own step; a drag step merges into the open step
/// of the same drag. Returns the undo depth.
fn push_edit(world: &mut World, edit: InlineAssetEdit) -> usize {
    let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    if edit.coalesce {
        stack.push(Box::new(edit));
    } else {
        stack.seal();
        stack.push(Box::new(edit));
        stack.seal();
    }
    stack.undo_depth()
}

// ---------------------------------------------------------------------------
// Save all and autosave
// ---------------------------------------------------------------------------

/// The registry entry: every inline document with unsaved edits is a row of Save all and is
/// autosaved with the bytes its save writes.
pub const PROVIDER: DocProvider = DocProvider {
    kind: KIND,
    dirty: provider_dirty,
    snapshot: provider_snapshot,
    restore: provider_restore,
};

fn provider_dirty(world: &World) -> Vec<DocKey> {
    let (Some(root), Some(docs)) = (
        crate::mesh_source::project_root(world),
        world.get_resource::<InlineAssetDocs>(),
    ) else {
        return Vec::new();
    };
    docs.dirty_paths().iter().map(|rel| doc_key(&root, rel)).collect()
}

fn provider_snapshot(world: &World) -> Vec<DocSnapshot> {
    let (Some(root), Some(docs)) = (
        crate::mesh_source::project_root(world),
        world.get_resource::<InlineAssetDocs>(),
    ) else {
        return Vec::new();
    };
    docs.docs
        .iter()
        .filter(|(_, doc)| doc.dirty())
        .map(|(rel, doc)| DocSnapshot {
            key: doc_key(&root, rel),
            bytes: doc.text.as_bytes().to_vec(),
        })
        .collect()
}

/// Reopens an autosaved material or physics material as an inline document with unsaved edits.
/// A data asset's autosave is not restored here (its value needs the file's type): open the file
/// in the Data Asset panel.
fn provider_restore(world: &mut World, key: &DocKey, bytes: &[u8]) -> Result<(), String> {
    let root = project_root(world).map_err(|e| e.message)?;
    let original = key
        .original
        .as_deref()
        .ok_or_else(|| "an inline asset always has a file".to_owned())?;
    let rel = original
        .strip_prefix(&root)
        .map(crate::content::slash_path)
        .map_err(|_| format!("{} is outside the project", original.display()))?;
    let kind = InlineKind::of_path(&rel).ok_or_else(|| format!("{rel} is not edited inline"))?;
    if kind == InlineKind::DataAsset {
        return Err(format!(
            "{rel}: reopen a data asset's autosave from the Data Asset panel"
        ));
    }
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    let (value, canonical) = decode_text(kind, text).map_err(|e| e.message)?;
    // The file's own text is the clean state; a missing or unreadable file keeps the doc dirty.
    let saved = std::fs::read_to_string(root.join(&rel))
        .ok()
        .and_then(|disk| decode_text(kind, &disk).ok())
        .map(|(_, text)| text)
        .unwrap_or_default();
    ensure_provider(world);
    let mut docs = world.get_resource_or_insert_with(InlineAssetDocs::default);
    docs.docs.insert(
        rel.clone(),
        InlineDoc {
            kind,
            value,
            text: canonical,
            saved_text: saved,
            data: None,
        },
    );
    docs.errors.remove(&rel);
    docs.refresh_view(&rel);
    Ok(())
}

// ---------------------------------------------------------------------------
// The Inspector side
// ---------------------------------------------------------------------------

/// What an inline section asked for this frame (run after the draw, never for a pass egui
/// discards).
#[derive(Debug, Clone, PartialEq)]
pub enum InlineRequest {
    /// Open (or re-read, when clean) the document of a path.
    Open(String),
    /// Set one field of a document.
    Set {
        /// The project-relative path.
        path: String,
        /// The dotted field path in the JSON view.
        field: String,
        /// The new value.
        value: Value,
        /// A drag step (merges with the previous step of the same drag).
        coalesce: bool,
    },
    /// A drag or a typed text ended: the next edit is a new undo step.
    Seal,
    /// Save one document to its file.
    Save(String),
}

/// Carries out one request of an inline section.
pub fn run_request(world: &mut World, request: InlineRequest) {
    let outcome = match request {
        InlineRequest::Open(path) => open(world, &path).map(|_| ()),
        InlineRequest::Set {
            path,
            field,
            value,
            coalesce,
        } => set_field(world, &path, &field, value, coalesce).map(|_| ()),
        InlineRequest::Seal => {
            if let Some(mut stack) = world.get_resource_mut::<UndoStack>() {
                stack.seal();
            }
            Ok(())
        }
        InlineRequest::Save(path) => save(world, &path).map(|_| ()),
    };
    if let Err(e) = outcome {
        tracing::debug!(target: "regia_editor", "inline asset: {} ({})", e.message, e.code);
    }
}

/// One edit a field widget made this frame.
struct WidgetEdit {
    field: String,
    value: Value,
    coalesce: bool,
}

/// The widgets of one JSON value, recursively. Every widget is named `<asset field> <field path>`.
fn value_ui(
    ui: &mut egui::Ui,
    value: &Value,
    field: &str,
    name: &str,
    id: egui::Id,
    edits: &mut Vec<WidgetEdit>,
    seal: &mut bool,
) {
    let spoken = if field.is_empty() {
        name.to_owned()
    } else {
        format!("{name} {field}")
    };
    match value {
        Value::Null => {
            ui.weak("none");
        }
        Value::Bool(on) => {
            let mut on = *on;
            if crate::a11y_audit::bare_checkbox(ui, &mut on, &spoken).changed() {
                edits.push(WidgetEdit {
                    field: field.to_owned(),
                    value: Value::Bool(on),
                    coalesce: false,
                });
            }
        }
        Value::Number(number) => {
            let integral = number.is_i64() || number.is_u64();
            let mut v = number.as_f64().unwrap_or_default();
            let response = ui.add(
                egui::DragValue::new(&mut v)
                    .speed(if integral { 1.0 } else { 0.01 })
                    .max_decimals(if integral { 0 } else { 4 }),
            );
            let dragging = response.dragged();
            if response.drag_stopped() || response.lost_focus() {
                *seal = true;
            }
            let response = crate::a11y_audit::named_drag_value(response, &spoken, v);
            if response.changed() && v.is_finite() {
                let value = if integral {
                    // An integer field stays an integer (rounded, the type checks its range).
                    json!(v.round() as i64)
                } else {
                    json!(v)
                };
                edits.push(WidgetEdit {
                    field: field.to_owned(),
                    value,
                    coalesce: dragging,
                });
            }
        }
        Value::String(text) => {
            let mut text = text.clone();
            let response = ui.add(egui::TextEdit::singleline(&mut text).desired_width(180.0));
            let response = crate::a11y_audit::name(response, egui::WidgetType::TextEdit, &spoken);
            if response.lost_focus() {
                *seal = true;
            }
            if response.changed() {
                // Typing is one step per field until the field loses focus.
                edits.push(WidgetEdit {
                    field: field.to_owned(),
                    value: Value::String(text),
                    coalesce: true,
                });
            }
        }
        Value::Array(items) => {
            let short_numbers = items.len() <= 4 && items.iter().all(Value::is_number);
            if short_numbers {
                ui.horizontal(|ui| {
                    for (i, item) in items.iter().enumerate() {
                        let child = join(field, &i.to_string());
                        value_ui(ui, item, &child, name, id, edits, seal);
                    }
                });
            } else if items.is_empty() {
                ui.weak("empty");
            } else {
                // The caller's header holds the list (an object's key): one row per item.
                ui.vertical(|ui| {
                    for (i, item) in items.iter().enumerate() {
                        let child = join(field, &i.to_string());
                        ui.horizontal(|ui| {
                            ui.label(i.to_string());
                            value_ui(ui, item, &child, name, id, edits, seal);
                        });
                    }
                });
            }
        }
        Value::Object(map) => {
            egui::Grid::new(id.with(("inline_grid", field.to_owned())))
                .num_columns(2)
                .striped(false)
                .show(ui, |ui| {
                    for (key, item) in map {
                        let child = join(field, key);
                        ui.label(key);
                        if field.is_empty() && key == "format_version" {
                            // The file's version is the format's, not an editable value.
                            ui.weak(item.to_string());
                        } else if item.is_object() || (item.is_array() && !short_number_list(item)) {
                            // The asset's own groups start open; deeper ones on demand.
                            egui::CollapsingHeader::new(group_summary(item))
                                .id_salt(id.with(("inline_object", child.clone())))
                                .default_open(field.is_empty())
                                .show(ui, |ui| value_ui(ui, item, &child, name, id, edits, seal));
                        } else {
                            value_ui(ui, item, &child, name, id, edits, seal);
                        }
                        ui.end_row();
                    }
                });
        }
    }
}

/// What a group's header says: `3 fields`, `1 item`.
fn group_summary(value: &Value) -> String {
    let (count, noun) = match value {
        Value::Object(map) => (map.len(), "field"),
        Value::Array(items) => (items.len(), "item"),
        _ => (0, "value"),
    };
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

fn short_number_list(value: &Value) -> bool {
    value
        .as_array()
        .is_some_and(|items| items.len() <= 4 && items.iter().all(Value::is_number))
}

fn join(parent: &str, segment: &str) -> String {
    if parent.is_empty() {
        segment.to_owned()
    } else {
        format!("{parent}.{segment}")
    }
}

/// Whether the asset at `rel` can be expanded in the Inspector.
#[must_use]
pub fn is_inline(rel: &str) -> bool {
    InlineKind::of_path(rel).is_some()
}

/// The expand arrow of an asset field whose asset is edited inline: returns whether the field is
/// expanded. Opening it (re)reads the document.
pub fn expand_toggle(
    ui: &mut egui::Ui,
    rel: &str,
    name: &str,
    id: egui::Id,
    links: &mut PickerLinks,
) -> bool {
    let open_id = id.with("inline_asset_open");
    // (open, the frame of the last toggle): egui may run a frame's UI twice with the same input,
    // and a click must toggle once.
    let (mut open, toggled_at) = ui
        .data(|d| d.get_temp::<(bool, u64)>(open_id))
        .unwrap_or((false, u64::MAX));
    let (glyph, verb) = if open {
        ("\u{25bc}", "Collapse")
    } else {
        ("\u{25b6}", "Expand")
    };
    let response = crate::a11y_audit::glyph_button(ui, glyph, &format!("{verb} {name}"));
    let frame = ui.ctx().cumulative_frame_nr();
    if response.clicked() {
        if toggled_at != frame {
            open = !open;
        }
        if open {
            // Expanding (re)reads the document; a request is idempotent and runs once per kept pass.
            links.request(PickerRequest::Inline(InlineRequest::Open(rel.to_owned())));
        }
        ui.data_mut(|d| d.insert_temp(open_id, (open, frame)));
    }
    open
}

/// The expanded asset under an asset field: a header (the file, unsaved state, **Save**, the last
/// refusal) and the asset's fields. Edits are requests: the document changes after the draw.
pub fn inline_section(ui: &mut egui::Ui, rel: &str, name: &str, id: egui::Id, links: &mut PickerLinks) {
    let _span = tracing::trace_span!(target: "regia_editor", "inspector_inline_asset").entered();
    let Some(view) = links.inline.get(rel).cloned() else {
        // Not open yet (a field expanded in an earlier session): ask once per frame until it is.
        links.request(PickerRequest::Inline(InlineRequest::Open(rel.to_owned())));
        ui.weak(format!("Opening {rel}..."));
        return;
    };
    let mut edits = Vec::new();
    let mut seal = false;
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal(|ui| {
            let file = rel.rsplit('/').next().unwrap_or(rel);
            if view.dirty {
                ui.label(egui::RichText::new(format!("{file} *")).strong())
                    .on_hover_text(format!(
                        "{rel} has unsaved edits (File \u{25b8} Save all saves them)"
                    ));
            } else {
                ui.label(egui::RichText::new(file).strong()).on_hover_text(rel);
            }
            if view.kind.is_some() {
                let save = ui.add_enabled(view.dirty, egui::Button::new("Save"));
                let save =
                    crate::a11y_audit::name(save, egui::WidgetType::Button, &format!("Save {name} asset"));
                if save.clicked() {
                    links.request(PickerRequest::Inline(InlineRequest::Save(rel.to_owned())));
                }
            }
        });
        if let Some(error) = &view.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        if view.kind.is_some() {
            value_ui(
                ui,
                &view.value,
                "",
                name,
                id.with(("inline_asset", rel)),
                &mut edits,
                &mut seal,
            );
        }
    });
    for edit in edits {
        links.request(PickerRequest::Inline(InlineRequest::Set {
            path: rel.to_owned(),
            field: edit.field,
            value: edit.value,
            coalesce: edit.coalesce,
        }));
    }
    if seal {
        links.request(PickerRequest::Inline(InlineRequest::Seal));
    }
}

// ---------------------------------------------------------------------------
// Agent surface
// ---------------------------------------------------------------------------

/// The asset path an agent call names: `path`, or the value of `field` of `component` on
/// `entity` (the asset field the Inspector would expand).
fn requested_path(world: &World, params: &Value) -> Result<String, RpcError> {
    let path = params.get("path").and_then(Value::as_str);
    let on_field =
        params.get("entity").is_some() || params.get("component").is_some() || params.get("field").is_some();
    match (path, on_field) {
        (Some(path), false) => Ok(path.to_owned()),
        (None, true) => {
            let entity = crate::undo_agent::live_entity(world, params, "entity")?;
            let component = params.get("component").and_then(Value::as_str);
            let field = params.get("field").and_then(Value::as_str);
            let (Some(component), Some(field)) = (component, field) else {
                return Err(err(
                    "asset_inline_bad_params",
                    "give `entity`, `component` and `field` together",
                )
                .rpc(HINT));
            };
            let current = crate::command::read_property(world, entity, component, field)
                .map_err(|e| err("asset_inline_not_asset", e.to_string()).rpc(HINT))?;
            current
                .try_downcast_ref::<String>()
                .cloned()
                .filter(|v| !v.trim().is_empty())
                .ok_or_else(|| {
                    err(
                        "asset_inline_not_asset",
                        format!("`{component}.{field}` holds no asset path"),
                    )
                    .rpc(HINT)
                })
        }
        _ => Err(err(
            "asset_inline_bad_params",
            "give `path`, or `entity`, `component` and `field` (not both)",
        )
        .rpc(HINT)),
    }
}

/// The agent's view of one open document.
fn doc_json(world: &World, rel: &str) -> Result<Value, RpcError> {
    let root = project_root(world).map_err(|e| e.rpc(HINT))?;
    let doc = world
        .get_resource::<InlineAssetDocs>()
        .and_then(|docs| docs.get(rel))
        .ok_or_else(|| err("asset_inline_not_found", format!("{rel} is not open inline")).rpc(HINT))?;
    Ok(json!({
        "path": rel,
        "kind": doc.kind.name(),
        "value": doc.value,
        "dirty": doc.dirty(),
        "save_id": doc_key(&root, rel).id(),
    }))
}

/// `editor.inspect_asset_inline`.
fn inspect_asset_inline(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = requested_path(world, params)?;
    let rel = open(world, &path).map_err(|e| e.rpc(HINT))?;
    doc_json(world, &rel)
}

/// `editor.asset_inline_set`.
fn asset_inline_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| err("asset_inline_bad_params", "`path` (the asset) is required").rpc(HINT))?;
    let field = params.get("field").and_then(Value::as_str).ok_or_else(|| {
        err(
            "asset_inline_bad_params",
            "`field` (a dotted path, \"\" for the whole value) is required",
        )
        .rpc(HINT)
    })?;
    let value = params
        .get("value")
        .cloned()
        .ok_or_else(|| err("asset_inline_bad_params", "`value` is required").rpc(HINT))?;
    let set = set_field(world, path, field, value, false).map_err(|e| e.rpc(HINT))?;
    let root = project_root(world).map_err(|e| e.rpc(HINT))?;
    Ok(json!({
        "path": set.path,
        "field": field,
        "changed": set.changed,
        "before": set.before,
        "after": set.after,
        "dirty": set.dirty,
        "undoable": set.undoable,
        "save_id": doc_key(&root, &set.path).id(),
    }))
}

/// The inline asset methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.inspect_asset_inline",
                tool: "inspect_asset_inline",
                description: "What the Inspector's expand arrow shows for an asset field: the referenced data asset, \
                    material or physics material as its JSON view (the fields `editor.asset_inline_set` sets), \
                    whether it has unsaved edits, and the `save_id` `editor.save_all` saves it with. Name the \
                    asset by `path`, or by `entity` + `component` + `field` (the asset field). Opens the document \
                    (re-read from disk when it has no unsaved edits).",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "Project-relative asset path (assets/... or relative to assets/).",
                    },
                    ParamSchema {
                        name: "entity",
                        kind: "number",
                        required: false,
                        description: "The entity whose asset field names the asset.",
                    },
                    ParamSchema {
                        name: "component",
                        kind: "string",
                        required: false,
                        description: "The component type path of the asset field.",
                    },
                    ParamSchema {
                        name: "field",
                        kind: "string",
                        required: false,
                        description: "The asset field's reflect path (`material`).",
                    },
                ],
                result: "{ path, kind, value, dirty, save_id }",
                mutating: false,
                permission: Permission::Read,
            },
            inspect_asset_inline as Handler,
        ),
        (
            MethodSchema {
                name: "editor.asset_inline_set",
                tool: "asset_inline_set",
                description: "Set one field of an asset edited inline under an Inspector asset field (data asset, \
                    material, physics material), as one undo step. The edit stays in the editor (the asset is \
                    dirty in Save all) until `editor.save_all { ids: [save_id] }` writes the file; the level \
                    is not marked dirty. The whole document is checked by its typed reader first: a refused \
                    value changes nothing.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Project-relative asset path.",
                    },
                    ParamSchema {
                        name: "field",
                        kind: "string",
                        required: true,
                        description: "Dotted path in the JSON view (`base.metallic`, `base.base_color.0`); \"\" for the whole value.",
                    },
                    ParamSchema {
                        name: "value",
                        kind: "any",
                        required: true,
                        description: "The new value, in the JSON view's form.",
                    },
                ],
                result: "{ path, field, changed, before, after, dirty, undoable, save_id }",
                mutating: true,
                permission: Permission::Assets,
            },
            asset_inline_set as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use regia_ecs::reflect::Reflect;
    use regia_ecs::reflect::std_traits::ReflectDefault;
    use regia_render::MeshSource;
    use regia_scene::prelude::{Name, Transform};

    use super::*;
    use crate::selection::Selection;

    fn app(root: &Path) -> regia_app::App {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.register_type::<MeshSource>();
        app.init_resource::<UndoStack>();
        app.world_mut().insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.to_path_buf()),
        });
        app
    }

    /// A project with a standard material at `assets/materials/rock.material.ron`.
    fn project(label: &str) -> (PathBuf, String) {
        let dir = regia_test_utils::unique_temp_dir(label);
        std::fs::create_dir_all(dir.join("assets/materials")).expect("dir");
        let created = crate::content::create_asset(
            &dir,
            crate::content::NewAssetKind::Material,
            "assets/materials/rock.material.ron",
        )
        .expect("material");
        (dir, created)
    }

    fn material_on_disk(root: &Path, rel: &str) -> regia_asset::MaterialAssetRon {
        let text = std::fs::read(root.join(rel)).expect("read material");
        regia_asset::MaterialAssetRon::from_ron(&text).expect("material parses")
    }

    fn metallic(world: &World, rel: &str) -> f64 {
        world
            .resource::<InlineAssetDocs>()
            .get(rel)
            .and_then(|doc| doc.value()["base"]["metallic"].as_f64())
            .expect("metallic in the inline document")
    }

    fn undo_depth(world: &World) -> usize {
        world.resource::<UndoStack>().undo_depth()
    }

    fn frame(
        ctx: &egui::Context,
        world: &mut World,
        events: Vec<egui::Event>,
    ) -> egui::accesskit::TreeUpdate {
        ctx.enable_accesskit();
        ctx.all_styles_mut(|style| style.animation_time = 0.0);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1100.0, 2400.0),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| crate::inspector::inspector_panel(ui, world));
        output.textures_delta.clear();
        output
            .platform_output
            .accesskit_update
            .unwrap_or_else(|| egui::accesskit::TreeUpdate {
                nodes: Vec::new(),
                tree: None,
                focus: egui::accesskit::NodeId(0),
                tree_id: egui::accesskit::TreeId::ROOT,
            })
    }

    fn labels(update: &egui::accesskit::TreeUpdate) -> Vec<String> {
        update
            .nodes
            .iter()
            .filter_map(|(_, node)| node.label().map(str::to_owned))
            .collect()
    }

    fn node_center(update: &egui::accesskit::TreeUpdate, text: &str) -> Option<egui::Pos2> {
        update.nodes.iter().find_map(|(_, node)| {
            let bounds = node.bounds()?;
            (node.label() == Some(text)).then(|| {
                egui::pos2(
                    ((bounds.x0 + bounds.x1) / 2.0) as f32,
                    ((bounds.y0 + bounds.y1) / 2.0) as f32,
                )
            })
        })
    }

    fn focused_label(update: &egui::accesskit::TreeUpdate) -> Option<String> {
        update
            .nodes
            .iter()
            .find(|(id, _)| *id == update.focus)
            .and_then(|(_, node)| node.label().map(str::to_owned))
    }

    fn keys(key: egui::Key) -> Vec<egui::Event> {
        [true, false]
            .into_iter()
            .map(|pressed| egui::Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            })
            .collect()
    }

    /// Opens the component header called `header` with a click.
    fn open_header(ctx: &egui::Context, world: &mut World, header: &str) {
        let tree = frame(ctx, world, Vec::new());
        let at = node_center(&tree, header).unwrap_or_else(|| panic!("{header}: {:?}", labels(&tree)));
        let press = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(ctx, world, vec![egui::Event::PointerMoved(at), press(true)]);
        frame(ctx, world, vec![press(false)]);
        frame(
            ctx,
            world,
            vec![egui::Event::PointerMoved(egui::pos2(1090.0, 2390.0))],
        );
    }

    /// Presses Tab until the focused widget is called `label` (the keyboard path).
    fn tab_to(ctx: &egui::Context, world: &mut World, label: &str) {
        let mut seen = Vec::new();
        for _ in 0..160 {
            let tree = frame(ctx, world, keys(egui::Key::Tab));
            let focus = focused_label(&tree);
            if focus.as_deref() == Some(label) {
                return;
            }
            seen.push(focus.unwrap_or_default());
        }
        panic!("Tab never reached {label:?}; focused in turn: {seen:?}");
    }

    fn tab_and_enter(ctx: &egui::Context, world: &mut World, label: &str) {
        tab_to(ctx, world, label);
        frame(ctx, world, keys(egui::Key::Enter));
        frame(ctx, world, Vec::new());
    }

    fn select(world: &mut World, entity: Entity) {
        let mut selection = Selection::default();
        selection.select(entity, crate::selection::SelectMode::Replace);
        world.insert_resource(selection);
    }

    /// The acceptance check of P-124: the expand arrow of a material field shows the material's
    /// fields; an edit from the keyboard is one undo step that leaves the level clean, marks the
    /// material dirty in Save all and does not touch the file; Save all writes it; an undo makes it
    /// dirty again and the inline **Save** writes it back.
    #[test]
    fn an_inline_material_edit_marks_the_material_dirty_and_saves() {
        let (dir, rel) = project("inline-material-save");
        let mut app = app(&dir);
        let world = app.world_mut();
        let entity = world
            .spawn((
                Name::new("Rock"),
                Transform::default(),
                MeshSource::new("builtin:cube", &rel),
            ))
            .id();
        select(world, entity);
        let ctx = egui::Context::default();
        open_header(&ctx, world, "MeshSource");
        tab_and_enter(&ctx, world, "Expand MeshSource.material");
        frame(&ctx, world, Vec::new());
        let tree = frame(&ctx, world, Vec::new());
        assert!(
            node_center(&tree, "MeshSource.material base.metallic").is_some(),
            "the material's fields show under the field: {:?}",
            labels(&tree)
        );
        let on_disk = material_on_disk(&dir, &rel).base.metallic;
        let before = metallic(world, &rel);
        let depth = undo_depth(world);
        tab_to(&ctx, world, "MeshSource.material base.metallic");
        frame(&ctx, world, keys(egui::Key::ArrowUp));
        frame(&ctx, world, Vec::new());
        let after = metallic(world, &rel);
        assert!(after > before, "arrow up raises metallic: {before} -> {after}");
        assert_eq!(undo_depth(world), depth + 1, "one undo step");
        assert!(
            !world.resource::<UndoStack>().is_dirty(),
            "the level stays clean: the edit is the material's"
        );
        assert!(
            (f64::from(material_on_disk(&dir, &rel).base.metallic) - f64::from(on_disk)).abs() < 1e-9,
            "the file is written by a save, not by the edit"
        );
        let dirty = crate::save_all_dialog::dirty_documents(world);
        let row = dirty
            .iter()
            .find(|doc| doc.kind == KIND)
            .unwrap_or_else(|| panic!("Save all lists the material: {dirty:?}"));
        assert_eq!(row.path.as_deref(), Some(rel.as_str()));
        assert!(row.can_save, "{row:?}");
        let outcome = crate::save_all_dialog::save_documents(world, std::slice::from_ref(&row.id));
        assert_eq!(outcome.saved, vec![row.id.clone()], "{outcome:?}");
        let saved = f64::from(material_on_disk(&dir, &rel).base.metallic);
        assert!((saved - after).abs() < 1e-6, "saved {saved}, edited {after}");
        assert!(
            crate::save_all_dialog::dirty_documents(world)
                .iter()
                .all(|doc| doc.kind != KIND),
            "saved: no longer dirty"
        );
        // Undo is the material's again: dirty, and the inline Save writes the old value.
        assert!(crate::command::undo_world(world));
        assert!((metallic(world, &rel) - before).abs() < 1e-9);
        assert!(
            world
                .resource::<InlineAssetDocs>()
                .get(&rel)
                .is_some_and(InlineDoc::dirty)
        );
        tab_and_enter(&ctx, world, "Save MeshSource.material asset");
        let restored = f64::from(material_on_disk(&dir, &rel).base.metallic);
        assert!((restored - before).abs() < 1e-6, "{restored} vs {before}");
        assert!(
            !world
                .resource::<InlineAssetDocs>()
                .get(&rel)
                .is_some_and(InlineDoc::dirty)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Make unique copies the asset next to it and repoints the field as one undo step; undo points
    /// the field back, redo to the copy again. Unsaved inline edits stay with the original file's
    /// document; the copy opens as its own (clean) document.
    #[test]
    fn make_unique_repoints_the_field_and_undoes_in_one_step() {
        let (dir, rel) = project("inline-make-unique");
        let mut app = app(&dir);
        let world = app.world_mut();
        let entity = world
            .spawn((
                Name::new("Rock"),
                Transform::default(),
                MeshSource::new("builtin:cube", &rel),
            ))
            .id();
        let edited = set_field(world, &rel, "base.roughness", json!(0.25), false).expect("inline edit");
        assert!(edited.changed && edited.dirty);
        let depth = undo_depth(world);
        let made = crate::inspector::make_unique::make_unique(world, entity, "MeshSource", "material")
            .expect("make unique");
        assert_eq!(made.to, "assets/materials/rock_unique.material.ron");
        assert!(dir.join(&made.to).is_file());
        assert_eq!(made.undoable, depth + 1, "one undo step");
        let field = |world: &World| world.get::<MeshSource>(entity).map(|m| m.material.clone());
        assert_eq!(field(world).as_deref(), Some(made.value.as_str()));
        // The original's unsaved edit is still the original's.
        assert!(
            world
                .resource::<InlineAssetDocs>()
                .get(&rel)
                .is_some_and(InlineDoc::dirty)
        );
        let copy = open(world, &made.to).expect("open the copy");
        let copy_doc = world.resource::<InlineAssetDocs>().get(&copy).expect("copy doc");
        assert!(!copy_doc.dirty(), "the copy is the file as saved");
        assert_ne!(copy_doc.value()["base"]["roughness"], json!(0.25));
        assert!(crate::command::undo_world(world));
        assert_eq!(field(world).as_deref(), Some(rel.as_str()), "undo points back");
        assert_eq!(undo_depth(world), depth);
        assert!(crate::command::redo_world(world));
        assert_eq!(
            field(world).as_deref(),
            Some(made.value.as_str()),
            "redo repoints"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn call(app: &mut regia_app::App, method: &str, params: Value) -> Result<Value, RpcError> {
        let handler = crate::agent_ext::all_methods()
            .into_iter()
            .find(|(schema, _)| schema.name == method)
            .map(|(_, handler)| handler)
            .unwrap_or_else(|| panic!("{method} is registered"));
        handler(app.world_mut(), &params)
    }

    fn reason(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .unwrap_or_default()
            .to_owned()
    }

    /// The two methods through the editor's method table, saved with `editor.save_all`, with their
    /// refusals; a physics material checks its ranges.
    #[test]
    fn the_inline_asset_methods_dispatch() {
        let (dir, rel) = project("inline-methods");
        let physmat = regia_physics::PhysicsMaterial::new("ice")
            .to_ron()
            .expect("physmat");
        std::fs::write(dir.join("assets/materials/ice.physmat.ron"), physmat).expect("write physmat");
        std::fs::write(dir.join("assets/materials/rock.png"), b"not a png").expect("png");
        let mut app = app(&dir);
        let entity = app.world_mut().spawn(MeshSource::new("builtin:cube", &rel)).id();
        let bits = entity.to_bits();
        let v = call(
            &mut app,
            "editor.inspect_asset_inline",
            json!({ "entity": bits, "component": "MeshSource", "field": "material" }),
        )
        .expect("inspect");
        assert_eq!(v["path"], json!(rel));
        assert_eq!(v["kind"], "material");
        assert_eq!(v["dirty"], false);
        assert!(v["value"]["base"]["roughness"].is_number(), "{v}");
        let save_id = v["save_id"].as_str().expect("save_id").to_owned();

        let v = call(
            &mut app,
            "editor.asset_inline_set",
            json!({ "path": rel, "field": "base.roughness", "value": 0.3 }),
        )
        .expect("set");
        assert_eq!(v["changed"], true);
        assert_eq!(v["dirty"], true);
        assert_eq!(v["save_id"], json!(save_id));
        assert!(
            (v["after"].as_f64().unwrap_or_default() - 0.3).abs() < 1e-6,
            "{v}"
        );
        let set = |app: &mut regia_app::App, params: Value| call(app, "editor.asset_inline_set", params);
        let refusals = [
            (
                json!({ "path": rel, "field": "base.nope", "value": 1 }),
                "asset_inline_no_field",
            ),
            (
                json!({ "path": rel, "field": "base.roughness", "value": "rough" }),
                "asset_inline_bad_value",
            ),
            (
                json!({ "path": "assets/materials/rock.png", "field": "", "value": 1 }),
                "asset_inline_unsupported",
            ),
            (
                json!({ "path": "../outside.material.ron", "field": "a", "value": 1 }),
                "asset_inline_bad_path",
            ),
            (
                json!({ "path": "assets/materials/gone.material.ron", "field": "a", "value": 1 }),
                "asset_inline_not_found",
            ),
            (json!({ "path": rel, "value": 1 }), "asset_inline_bad_params"),
            (
                json!({ "path": "assets/materials/ice.physmat.ron", "field": "friction", "value": 5.0 }),
                "asset_inline_bad_value",
            ),
        ];
        for (params, code) in refusals {
            let e = set(&mut app, params.clone()).expect_err("refused");
            assert_eq!(reason(&e), code, "{params}: {e:?}");
        }
        let e = call(
            &mut app,
            "editor.inspect_asset_inline",
            json!({ "path": rel, "entity": bits, "component": "MeshSource", "field": "material" }),
        )
        .expect_err("both forms");
        assert_eq!(reason(&e), "asset_inline_bad_params");
        let e = call(
            &mut app,
            "editor.inspect_asset_inline",
            json!({ "entity": bits, "component": "Transform", "field": "translation" }),
        )
        .expect_err("not an asset field");
        assert_eq!(reason(&e), "asset_inline_not_asset");
        // A refused value changed nothing: the roughness is still the accepted edit.
        let v = call(&mut app, "editor.inspect_asset_inline", json!({ "path": rel })).expect("again");
        assert!((v["value"]["base"]["roughness"].as_f64().unwrap_or_default() - 0.3).abs() < 1e-6);
        assert_eq!(v["dirty"], true, "an inspect keeps unsaved edits");

        let v = set(
            &mut app,
            json!({ "path": "assets/materials/ice.physmat.ron", "field": "friction", "value": 0.1 }),
        )
        .expect("physmat set");
        assert_eq!(v["dirty"], true);
        let v = call(&mut app, "editor.save_all", json!({ "ids": [save_id] })).expect("save_all");
        assert_eq!(v["saved"], json!([save_id]), "{v}");
        let saved = material_on_disk(&dir, &rel).base.roughness;
        assert!((f64::from(saved) - 0.3).abs() < 1e-6);
        // The physics material is still dirty (only the material was saved).
        let docs = app.world().resource::<InlineAssetDocs>();
        assert_eq!(
            docs.dirty_paths(),
            vec!["assets/materials/ice.physmat.ron".to_owned()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A game-style data asset type (registered as one).
    #[derive(Reflect, Default, Clone, PartialEq, Debug)]
    #[reflect(Default)]
    struct LootTable {
        rolls: u32,
        label: String,
    }
    impl regia_asset::data_asset::Asset for LootTable {}

    /// A data asset edits inline through its reflected type: a value the type refuses changes
    /// nothing, an accepted one is one undo step and dirty until saved, and the save writes the
    /// data asset file a game loads.
    #[test]
    fn a_data_asset_edits_inline_through_its_reflected_type() {
        let dir = regia_test_utils::unique_temp_dir("inline-data-asset");
        let mut app = app(&dir);
        app.register_type::<LootTable>();
        let registry = app.world().resource::<AppTypeRegistry>().clone();
        registry
            .write()
            .register_type_data::<LootTable, regia_asset::data_asset::ReflectAsset>();
        let rel = {
            let guard = registry.read();
            crate::data_asset::create_data_asset(&dir, "loot/table.data.ron", "LootTable", &guard)
                .expect("create")
        };
        let world = app.world_mut();
        let opened = open(world, &rel).expect("open");
        assert_eq!(opened, rel);
        let doc = world.resource::<InlineAssetDocs>().get(&rel).expect("doc");
        assert_eq!(doc.kind, InlineKind::DataAsset);
        assert_eq!(doc.value()["rolls"], json!(0));
        let e = set_field(world, &rel, "rolls", json!("many"), false).expect_err("not a number");
        assert_eq!(e.code, "asset_inline_bad_value");
        assert!(
            !world
                .resource::<InlineAssetDocs>()
                .get(&rel)
                .is_some_and(InlineDoc::dirty)
        );
        let set = set_field(world, &rel, "rolls", json!(3), false).expect("rolls");
        assert!(set.changed && set.dirty);
        assert_eq!(set.after, json!(3));
        save(world, &rel).expect("save");
        let reread = {
            let guard = registry.read();
            DataAssetDoc::open(&dir, &rel, &guard)
                .expect("reopen")
                .value_json(&guard)
        };
        assert_eq!(reread["rolls"], json!(3));
        assert!(crate::command::undo_world(world));
        let doc = world.resource::<InlineAssetDocs>().get(&rel).expect("doc");
        assert_eq!(doc.value()["rolls"], json!(0));
        assert!(doc.dirty(), "undo after the save: the file has 3, the document 0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn paths_stay_in_the_project_and_fields_address_the_json_view() {
        assert_eq!(
            normalize_path("materials/a.material.ron").expect("relative"),
            "assets/materials/a.material.ron"
        );
        for bad in [
            "",
            "builtin:cube",
            "assets/../x.material.ron",
            "C:/x.material.ron",
            "assets//x.data.ron",
        ] {
            assert_eq!(
                normalize_path(bad).expect_err(bad).code,
                "asset_inline_bad_path",
                "{bad}"
            );
        }
        let mut view = json!({ "base": { "color": [0.1, 0.2, 0.3, 1.0] }, "name": "x" });
        set_at(&mut view, "base.color.2", json!(0.9)).expect("index");
        assert_eq!(view["base"]["color"][2], json!(0.9));
        assert_eq!(
            set_at(&mut view, "base.color.9", json!(0))
                .expect_err("out of range")
                .code,
            "asset_inline_no_field"
        );
        assert_eq!(
            set_at(&mut view, "name.inner", json!(0)).expect_err("leaf").code,
            "asset_inline_no_field"
        );
        assert_eq!(value_at(&view, "base.color.0"), Some(&json!(0.1)));
        assert_eq!(InlineKind::of_path("A.MATERIAL.RON"), Some(InlineKind::Material));
        assert_eq!(InlineKind::of_path("x.matgraph.ron"), None);
    }
}
