// SPDX-License-Identifier: MPL-2.0
//! Data assets and their editor (`docs/specs/19-editor.md` § 2, ADR-7234).
//!
//! A data asset is one reflected value saved as `<name>.data.ron`:
//!
//! ```ron
//! DataAsset(
//!     format_version: 1,
//!     type: "my_game::loot::LootTable",
//!     value: (rolls: 3, weights: [1.0, 2.0]),
//! )
//! ```
//!
//! The envelope is the runtime's (`regia_asset::data_asset`, `docs/formats/data-ron.md`, ADR-7232):
//! the cook checks it and a game loads it (ADR-7310 reconciled the editor's earlier `type_path`
//! spelling, which this module still reads). `value` is the reflect serializer's RON for the type
//! (the same form a level stores a component in). A type a game loads — one registered as a data
//! asset (a `#[derive(Asset)]` type registered with `register_data_asset` or `#[reflect(Asset)]`;
//! the derive alone implements the trait but registers nothing) — can be a data asset; fields the file
//! leaves out keep the type's defaults. The Data Asset panel draws the value with the inspector's
//! generic reflection widgets; every edit is one undo step. The Content Browser's New ▸ Data asset
//! entry and `editor.create_asset { kind: "data_asset", type }` create one, and the
//! `editor.data_asset_*` methods open, edit and save it the way the panel does
//! (`book/agents/editor-creators.md`).

use std::path::Path;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset::data_asset::ReflectAsset;
use regia_ecs::prelude::*;
use regia_ecs::reflect::serde::{TypedReflectDeserializer, TypedReflectSerializer};
use regia_ecs::reflect::{
    AppTypeRegistry, GetPath, PartialReflect, Reflect, ReflectComponent, ReflectResource, TypeInfo,
    TypeRegistry,
};
use serde::de::DeserializeSeed as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::command::{EditorCommand, UndoStack};
use crate::data_panels::PanelError;
use crate::inspector::{InspectorContext, InspectorUi};
use crate::viewport_agent::Handler;

/// The file extension of a data asset (without the leading dot).
pub const DATA_ASSET_EXTENSION: &str = "data.ron";

/// The current `.data.ron` format version.
pub const DATA_ASSET_FORMAT_VERSION: u32 = 1;

/// Type path prefixes the New ▸ Data asset list leaves out: the standard library and math types
/// are building blocks, not assets. They can still be named explicitly.
const HIDDEN_PREFIXES: &[&str] = &[
    "core::",
    "std::",
    "alloc::",
    "glam::",
    "bevy_",
    "smallvec::",
    "uuid::",
    "petgraph::",
];

/// The `.data.ron` file shape: the runtime envelope `DataAsset(format_version, type, value)`
/// (`regia_asset::data_asset`, ADR-7232). The editor's first spelling of the type key,
/// `type_path`, is still read (ADR-7310).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename = "DataAsset", deny_unknown_fields)]
pub struct DataAssetFile {
    /// Must be at most [`DATA_ASSET_FORMAT_VERSION`].
    pub format_version: u32,
    /// The reflected type of `value` (its registered type path), written as `type`.
    #[serde(rename = "type", alias = "type_path")]
    pub type_path: String,
    /// The value in the reflect serializer's RON form.
    pub value: Box<ron::value::RawValue>,
}

impl DataAssetFile {
    /// Parses `.data.ron` text (the value is not checked against its type here).
    ///
    /// # Errors
    ///
    /// `data_asset_parse` when the text is not the file shape, `data_asset_version` for a version
    /// newer than this build.
    pub fn from_ron_str(text: &str) -> Result<Self, PanelError> {
        let file: Self =
            ron::from_str(text).map_err(|e| err("data_asset_parse", format!("not a data asset: {e}")))?;
        if file.format_version == 0 || file.format_version > DATA_ASSET_FORMAT_VERSION {
            return Err(err(
                "data_asset_version",
                format!(
                    "format_version {} is not one this build reads (1..={DATA_ASSET_FORMAT_VERSION})",
                    file.format_version
                ),
            ));
        }
        Ok(file)
    }

    /// The file text: pretty RON, the envelope named `DataAsset`, with the value indented under
    /// `value:`.
    ///
    /// # Errors
    ///
    /// `data_asset_serialize` when RON cannot write the record.
    pub fn to_ron_string(&self) -> Result<String, PanelError> {
        let text = ron::ser::to_string_pretty(self, regia_core::ron_pretty().struct_names(true))
            .map_err(|e| err("data_asset_serialize", e.to_string()))?;
        Ok(if text.ends_with('\n') {
            text
        } else {
            format!("{text}\n")
        })
    }
}

fn err(code: &str, message: impl Into<String>) -> PanelError {
    PanelError::new(code, message)
}

/// The registered type paths the New ▸ Data asset list offers: the data asset types a game loads
/// (`#[derive(Asset)]` types registered with `RegisterDataAssetExt::register_data_asset` or
/// `#[reflect(Asset)]`; the derive alone implements the trait but registers nothing) that are
/// neither components nor resources, outside the standard library and math crates. Sorted.
#[must_use]
pub fn data_asset_types(registry: &TypeRegistry) -> Vec<String> {
    let mut out: Vec<String> = registry
        .iter()
        .filter(|reg| reg.data::<ReflectAsset>().is_some())
        .filter(|reg| matches!(reg.type_info(), TypeInfo::Struct(_)))
        .filter(|reg| reg.data::<ReflectComponent>().is_none() && reg.data::<ReflectResource>().is_none())
        .map(|reg| reg.type_info().type_path().to_owned())
        .filter(|path| !HIDDEN_PREFIXES.iter().any(|prefix| path.starts_with(prefix)))
        .collect();
    out.sort();
    out
}

/// The default value of `type_path` (`data_asset_unknown_type`).
fn default_value(registry: &TypeRegistry, type_path: &str) -> Result<Box<dyn Reflect>, PanelError> {
    let reg = registry
        .get_with_type_path(type_path)
        .or_else(|| registry.get_with_short_type_path(type_path))
        .ok_or_else(|| {
            err(
                "data_asset_unknown_type",
                format!("`{type_path}` is not a registered type"),
            )
        })?;
    // A game loads only types registered as data assets (ADR-7310 (11)); the editor offers and
    // creates only those too, so it cannot write a file the runtime loader would refuse.
    let asset = reg.data::<ReflectAsset>().ok_or_else(|| {
        err(
            "data_asset_unknown_type",
            format!(
                "`{}` is not a registered data asset type (register its #[derive(Asset)] type with \
                 register_data_asset or #[reflect(Asset)]); a game could not load the file",
                reg.type_info().type_path()
            ),
        )
    })?;
    Ok(asset.default_value())
}

/// The reflect serializer's RON for `value`, pretty-printed.
fn value_ron(value: &dyn PartialReflect, registry: &TypeRegistry) -> Result<String, PanelError> {
    let serializer = TypedReflectSerializer::new(value, registry);
    ron::ser::to_string_pretty(&serializer, regia_core::ron_pretty())
        .map_err(|e| err("data_asset_serialize", e.to_string()))
}

/// Parses `ron` as `type_path`'s value over its defaults.
fn parse_value(
    registry: &TypeRegistry,
    type_path: &str,
    ron_text: &str,
) -> Result<Box<dyn Reflect>, PanelError> {
    let mut value = default_value(registry, type_path)?;
    let reg = registry
        .get_with_type_path(value.reflect_type_path())
        .ok_or_else(|| {
            err(
                "data_asset_unknown_type",
                format!("`{type_path}` is not registered"),
            )
        })?;
    let mut de = ron::Deserializer::from_str(ron_text)
        .map_err(|e| err("data_asset_bad_value", format!("value: {e}")))?;
    let dynamic = TypedReflectDeserializer::new(reg, registry)
        .deserialize(&mut de)
        .map_err(|e| {
            err(
                "data_asset_bad_value",
                format!("value does not fit `{type_path}`: {e}"),
            )
        })?;
    value
        .as_partial_reflect_mut()
        .try_apply(dynamic.as_ref())
        .map_err(|e| {
            err(
                "data_asset_bad_value",
                format!("value does not fit `{type_path}`: {e}"),
            )
        })?;
    Ok(value)
}

/// The file text for `value` of `type_path` (the value RON indented under `value:`).
fn file_text(
    type_path: &str,
    value: &dyn PartialReflect,
    registry: &TypeRegistry,
) -> Result<String, PanelError> {
    // The pretty value is indented one level so it lines up under `value:`. RON writes newlines
    // inside strings as `\n` escapes, so every line break here is structure.
    let inner = value_ron(value, registry)?.replace('\n', "\n    ");
    let raw = ron::value::RawValue::from_ron(&inner)
        .map_err(|e| err("data_asset_serialize", e.to_string()))?
        .to_owned();
    DataAssetFile {
        format_version: DATA_ASSET_FORMAT_VERSION,
        type_path: type_path.to_owned(),
        value: raw,
    }
    .to_ron_string()
}

/// Checks a project-relative data asset path; returns `(rel without assets/, full path)`.
fn asset_path(root: &Path, path: &str) -> Result<(String, std::path::PathBuf), PanelError> {
    let rel = path.replace('\\', "/");
    let rel = rel.trim_start_matches("assets/").trim_matches('/').to_owned();
    if rel.is_empty()
        || rel.split('/').any(|s| s.is_empty() || s.starts_with('.'))
        || regia_core::project_path::check_relative(&rel).is_err()
    {
        return Err(err("data_asset_bad_path", format!("invalid asset path {path:?}")));
    }
    if !rel.ends_with(&format!(".{DATA_ASSET_EXTENSION}")) {
        return Err(err(
            "data_asset_bad_path",
            format!("the path must end with `.{DATA_ASSET_EXTENSION}`: {path}"),
        ));
    }
    let full = root.join("assets").join(&rel);
    Ok((rel, full))
}

/// Writes a new data asset holding `type_path`'s default value at project-relative `path`.
/// Returns the created path (`assets/...`).
///
/// # Errors
///
/// `data_asset_bad_path`, `data_asset_exists`, `data_asset_unknown_type` (not a registered data
/// asset type), or `data_asset_io`.
pub fn create_data_asset(
    root: &Path,
    path: &str,
    type_path: &str,
    registry: &TypeRegistry,
) -> Result<String, PanelError> {
    let (rel, full) = asset_path(root, path)?;
    if full.exists() {
        return Err(err("data_asset_exists", format!("assets/{rel} already exists")));
    }
    let value = default_value(registry, type_path)?;
    let canonical = value.reflect_type_path().to_owned();
    let text = file_text(&canonical, value.as_partial_reflect(), registry)?;
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            err(
                "data_asset_io",
                format!("cannot create {}: {e}", parent.display()),
            )
        })?;
    }
    crate::atomic_write::write_atomic(&full, text.as_bytes())
        .map_err(|e| err("data_asset_io", format!("cannot write {}: {e}", full.display())))?;
    Ok(format!("assets/{rel}"))
}

/// One open data asset: the edited value and the text last read or saved.
pub struct DataAssetDoc {
    /// Project-relative path (`assets/...`).
    pub path: String,
    /// The value's type path.
    pub type_path: String,
    value: Box<dyn Reflect>,
    saved_text: String,
}

impl std::fmt::Debug for DataAssetDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataAssetDoc")
            .field("path", &self.path)
            .field("type_path", &self.type_path)
            .finish_non_exhaustive()
    }
}

impl DataAssetDoc {
    /// Reads `path` (project-relative) and parses its value.
    ///
    /// # Errors
    ///
    /// `data_asset_bad_path`, `data_asset_not_found`, `data_asset_parse`, `data_asset_version`,
    /// `data_asset_unknown_type` or `data_asset_bad_value`.
    pub fn open(root: &Path, path: &str, registry: &TypeRegistry) -> Result<Self, PanelError> {
        let (rel, full) = asset_path(root, path)?;
        let text = std::fs::read_to_string(&full)
            .map_err(|e| err("data_asset_not_found", format!("cannot read assets/{rel}: {e}")))?;
        let file = DataAssetFile::from_ron_str(&text)?;
        let value = parse_value(registry, &file.type_path, file.value.get_ron())?;
        let type_path = value.reflect_type_path().to_owned();
        // `dirty` compares the text a save would write, so the baseline is the opened value in
        // that same canonical form: a hand-written or compact file is not "unsaved" only because
        // its layout differs (the bulk-edit grid's Close and undo check this). A file at an
        // older version keeps its own text, so it stays dirty until saved at the current one.
        let saved_text = if file.format_version == DATA_ASSET_FORMAT_VERSION {
            file_text(&type_path, value.as_partial_reflect(), registry).unwrap_or(text)
        } else {
            text
        };
        Ok(Self {
            path: format!("assets/{rel}"),
            type_path,
            value,
            saved_text,
        })
    }

    /// The value.
    #[must_use]
    pub fn value(&self) -> &dyn Reflect {
        self.value.as_ref()
    }

    /// The value as JSON (the reflect serializer's form, like `editor.inspect_selection`).
    #[must_use]
    pub fn value_json(&self, registry: &TypeRegistry) -> Value {
        let serializer = TypedReflectSerializer::new(self.value.as_partial_reflect(), registry);
        serde_json::to_value(&serializer).unwrap_or(Value::Null)
    }

    /// The text a save would write.
    ///
    /// # Errors
    ///
    /// `data_asset_serialize`.
    pub fn to_text(&self, registry: &TypeRegistry) -> Result<String, PanelError> {
        file_text(&self.type_path, self.value.as_partial_reflect(), registry)
    }

    /// Whether the value differs from the file.
    #[must_use]
    pub fn dirty(&self, registry: &TypeRegistry) -> bool {
        !matches!(self.to_text(registry), Ok(text) if text == self.saved_text)
    }

    /// Sets `field` (a reflect path such as `rolls` or `weights[1]`; empty for the whole value) from
    /// JSON in the reflect serializer's form. Returns the value RON before the edit (for undo).
    ///
    /// # Errors
    ///
    /// `data_asset_no_field` or `data_asset_bad_value`; the value is unchanged on error.
    pub fn set_field(
        &mut self,
        field: &str,
        json: &Value,
        registry: &TypeRegistry,
    ) -> Result<String, PanelError> {
        let before = value_ron(self.value.as_partial_reflect(), registry)?;
        let mut edited = parse_value(registry, &self.type_path, &before)?;
        let target: &mut dyn PartialReflect = if field.is_empty() {
            edited.as_partial_reflect_mut()
        } else {
            edited.as_reflect_mut().reflect_path_mut(field).map_err(|_| {
                err(
                    "data_asset_no_field",
                    format!("`{}` has no field `{field}`", self.type_path),
                )
            })?
        };
        let reg = target
            .get_represented_type_info()
            .and_then(|info| registry.get(info.type_id()))
            .ok_or_else(|| {
                err(
                    "data_asset_bad_value",
                    format!("`{field}` has no registered type"),
                )
            })?;
        let new = TypedReflectDeserializer::new(reg, registry)
            .deserialize(json)
            .map_err(|e| {
                err(
                    "data_asset_bad_value",
                    format!(
                        "value does not fit `{field}` ({}): {e}",
                        reg.type_info().type_path()
                    ),
                )
            })?;
        target.try_apply(new.as_ref()).map_err(|e| {
            err(
                "data_asset_bad_value",
                format!("value does not fit `{field}`: {e}"),
            )
        })?;
        self.value = edited;
        Ok(before)
    }

    /// Replaces the value with `ron_text` (the undo path).
    fn set_value_ron(&mut self, ron_text: &str, registry: &TypeRegistry) -> Result<(), PanelError> {
        self.value = parse_value(registry, &self.type_path, ron_text)?;
        Ok(())
    }

    /// Writes the file atomically. Returns the bytes written.
    ///
    /// # Errors
    ///
    /// `data_asset_serialize` or `data_asset_io`.
    pub fn save(&mut self, root: &Path, registry: &TypeRegistry) -> Result<usize, PanelError> {
        let text = self.to_text(registry)?;
        let (_, full) = asset_path(root, &self.path)?;
        crate::atomic_write::write_atomic(&full, text.as_bytes())
            .map_err(|e| err("data_asset_io", format!("cannot write {}: {e}", full.display())))?;
        let bytes = text.len();
        self.saved_text = text;
        Ok(bytes)
    }
}

/// The Data Asset panel's state (an edit-world resource).
#[derive(Resource, Default, Debug)]
pub struct DataAssetEditor {
    /// The open asset.
    pub doc: Option<DataAssetDoc>,
    /// The status line (last error or action).
    pub status: String,
}

/// The editor's type registry (a clone of the handle), or an internal error.
fn registry_of(world: &World) -> Result<AppTypeRegistry, PanelError> {
    world
        .get_resource::<AppTypeRegistry>()
        .cloned()
        .ok_or_else(|| err("data_asset_no_registry", "the type registry is not available"))
}

/// The open project's root (`data_asset_no_project`).
fn project_root_of(world: &World) -> Result<std::path::PathBuf, PanelError> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or_else(|| err("data_asset_no_project", "no project is open"))
}

/// Opens `path` in the Data Asset panel and asks the shell to bring the panel forward.
///
/// # Errors
///
/// `data_asset_no_project`, `data_asset_unsaved` when the open asset has unsaved edits, and the
/// [`DataAssetDoc::open`] codes.
pub fn open_data_asset(world: &mut World, path: &str) -> Result<(), PanelError> {
    let root = project_root_of(world)?;
    let registry = registry_of(world)?;
    let registry = registry.read();
    if let Some(open) = world
        .get_resource::<DataAssetEditor>()
        .and_then(|e| e.doc.as_ref())
        && open.dirty(&registry)
    {
        return Err(err(
            "data_asset_unsaved",
            format!("{} has unsaved edits; save it first", open.path),
        ));
    }
    let doc = DataAssetDoc::open(&root, path, &registry)?;
    drop(registry);
    let mut editor = world.get_resource_or_insert_with(DataAssetEditor::default);
    editor.status = format!("opened {}", doc.path);
    editor.doc = Some(doc);
    crate::editor_layout::LayoutRequests::push(
        world,
        crate::editor_layout::LayoutRequest::Open(crate::shell::Panel::DataAsset),
    );
    Ok(())
}

/// Sets a field of the open asset as one undo step. Returns whether the value changed.
///
/// # Errors
///
/// `data_asset_not_open` and the [`DataAssetDoc::set_field`] codes.
pub fn edit_data_asset(world: &mut World, field: &str, value: &Value) -> Result<bool, PanelError> {
    let registry = registry_of(world)?;
    let registry = registry.read();
    let mut editor = world
        .get_resource_mut::<DataAssetEditor>()
        .ok_or_else(|| err("data_asset_not_open", "no data asset is open"))?;
    let doc = editor
        .doc
        .as_mut()
        .ok_or_else(|| err("data_asset_not_open", "no data asset is open"))?;
    let before = doc.set_field(field, value, &registry)?;
    let after = value_ron(doc.value.as_partial_reflect(), &registry)?;
    let path = doc.path.clone();
    drop(registry);
    if before == after {
        return Ok(false);
    }
    push_edit(
        world,
        DataAssetEdit {
            path,
            before,
            after,
            field: field.to_owned(),
            coalesce: false,
        },
    );
    Ok(true)
}

/// Saves the open asset. Returns `(path, bytes)`.
///
/// # Errors
///
/// `data_asset_not_open`, `data_asset_no_project` and the [`DataAssetDoc::save`] codes.
pub fn save_data_asset(world: &mut World) -> Result<(String, usize), PanelError> {
    let root = project_root_of(world)?;
    let registry = registry_of(world)?;
    let registry = registry.read();
    let mut editor = world
        .get_resource_mut::<DataAssetEditor>()
        .ok_or_else(|| err("data_asset_not_open", "no data asset is open"))?;
    let doc = editor
        .doc
        .as_mut()
        .ok_or_else(|| err("data_asset_not_open", "no data asset is open"))?;
    let bytes = doc.save(&root, &registry)?;
    let path = doc.path.clone();
    editor.status = format!("saved {path}");
    Ok((path, bytes))
}

/// One data asset edit on the editor's undo stack: the value RON before and after. Continuous panel
/// edits of one field (a slider drag) coalesce into one entry, as the inspector's do.
struct DataAssetEdit {
    path: String,
    before: String,
    after: String,
    field: String,
    coalesce: bool,
}

impl DataAssetEdit {
    fn set(world: &mut World, path: &str, text: &str) {
        let Ok(registry) = registry_of(world) else {
            return;
        };
        let registry = registry.read();
        let Some(mut editor) = world.get_resource_mut::<DataAssetEditor>() else {
            return;
        };
        // Only the asset the edit was made on: another open asset is left alone.
        if let Some(doc) = editor.doc.as_mut().filter(|d| d.path == path)
            && let Err(e) = doc.set_value_ron(text, &registry)
        {
            tracing::warn!(target: "regia_editor", "data asset undo on {path}: {}", e.message);
        }
    }
}

impl EditorCommand for DataAssetEdit {
    fn activity_targets(&self) -> (Vec<u64>, Vec<String>) {
        (Vec::new(), vec![self.path.clone()])
    }

    fn apply(&mut self, world: &mut World) {
        Self::set(world, &self.path, &self.after);
    }
    fn revert(&mut self, world: &mut World) {
        Self::set(world, &self.path, &self.before);
    }
    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = crate::command::downcast_to::<DataAssetEdit>(next) else {
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
        // The level document is untouched; the panel tracks the asset's own dirty state.
        false
    }
    fn description(&self) -> String {
        if self.field.is_empty() {
            format!("Edit {}", self.path)
        } else {
            format!("Edit {}.{}", self.path, self.field)
        }
    }
}

fn push_edit(world: &mut World, edit: DataAssetEdit) {
    let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    if edit.coalesce {
        // A drag step merges into the previous step of the same drag.
        stack.push(Box::new(edit));
    } else {
        stack.seal();
        stack.push(Box::new(edit));
        stack.seal();
    }
}

/// Draws the Data Asset panel: the open asset's type, its value through the inspector's reflection
/// widgets, Save and Revert.
#[tracing::instrument(target = "regia_editor", skip_all, name = "data_asset_panel")]
pub fn data_asset_panel(ui: &mut egui::Ui, world: &mut World) {
    let Ok(registry) = registry_of(world) else {
        ui.weak("The type registry is not available.");
        return;
    };
    let (path, type_path, current, dirty, status) = {
        let registry = registry.read();
        let editor = world.get_resource::<DataAssetEditor>();
        let status = editor.map(|e| e.status.clone()).unwrap_or_default();
        match editor.and_then(|e| e.doc.as_ref()) {
            Some(doc) => (
                doc.path.clone(),
                doc.type_path.clone(),
                value_ron(doc.value.as_partial_reflect(), &registry).ok(),
                doc.dirty(&registry),
                status,
            ),
            None => {
                ui.strong("Data Asset");
                ui.weak(
                    "Double-click a .data.ron in the Content Browser, or create one with New ▸ Data asset.",
                );
                if !status.is_empty() {
                    ui.weak(status);
                }
                return;
            }
        }
    };
    ui.horizontal(|ui| {
        ui.strong(if dirty { format!("{path} *") } else { path.clone() });
        ui.weak(crate::inspector::egui_utils::pretty_type_name(&type_path));
    });
    let mut save = false;
    let mut revert = false;
    ui.horizontal(|ui| {
        save = ui.add_enabled(dirty, egui::Button::new("Save")).clicked();
        revert = ui
            .add_enabled(dirty, egui::Button::new("Revert"))
            .on_hover_text("Reload the file and drop unsaved edits")
            .clicked();
        if !status.is_empty() {
            ui.weak(&status);
        }
    });
    ui.separator();
    let Some(current) = current else {
        return;
    };
    let registry_guard = registry.read();
    let Ok(mut edited) = parse_value(&registry_guard, &type_path, &current) else {
        return;
    };
    let mut context = InspectorContext::new(&type_path);
    egui::ScrollArea::vertical().show(ui, |ui| {
        InspectorUi::new(&registry_guard, &mut context).ui_for_reflect_with_options(
            edited.as_partial_reflect_mut(),
            ui,
            egui::Id::new("data-asset"),
            &(),
        );
    });
    let changes = context.take_changes();
    let changed = !changes.is_empty();
    // One frame edits one field; a structural change is its own entry.
    let (field, coalesce) = changes
        .first()
        .map_or((String::new(), false), |c| (c.path.clone(), !c.discrete));
    let after = if changed {
        value_ron(edited.as_partial_reflect(), &registry_guard).ok()
    } else {
        None
    };
    drop(registry_guard);
    if let Some(after) = after.filter(|a| *a != current) {
        let set = world
            .get_resource_mut::<DataAssetEditor>()
            .and_then(|mut e| e.doc.as_mut().map(|d| d.set_value_ron(&after, &registry.read())));
        if let Some(Ok(())) = set {
            push_edit(
                world,
                DataAssetEdit {
                    path: path.clone(),
                    before: current,
                    after,
                    field,
                    coalesce,
                },
            );
        }
    }
    if save && let Err(e) = save_data_asset(world) {
        world.get_resource_or_insert_with(DataAssetEditor::default).status = e.message;
    }
    if revert {
        if let Some(mut editor) = world.get_resource_mut::<DataAssetEditor>() {
            editor.doc = None;
        }
        if let Err(e) = open_data_asset(world, &path) {
            world.get_resource_or_insert_with(DataAssetEditor::default).status = e.message;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Agent protocol
// ---------------------------------------------------------------------------------------------

const HINT: &str = "see book/agents/editor-creators.md";

fn doc_json(world: &World) -> Result<Value, RpcError> {
    let registry = registry_of(world).map_err(|e| e.rpc(HINT))?;
    let registry = registry.read();
    let doc = world
        .get_resource::<DataAssetEditor>()
        .and_then(|e| e.doc.as_ref())
        .ok_or_else(|| {
            err("data_asset_not_open", "no data asset is open").rpc("call editor.data_asset_open first")
        })?;
    Ok(json!({
        "path": doc.path,
        "type": doc.type_path,
        "value": doc.value_json(&registry),
        "dirty": doc.dirty(&registry),
    }))
}

/// `editor.data_asset_types`.
fn data_asset_types_method(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let registry = registry_of(world).map_err(|e| e.rpc(HINT))?;
    let types = data_asset_types(&registry.read());
    Ok(json!({ "types": types }))
}

/// `editor.data_asset_open { path, discard? }`.
fn data_asset_open_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| {
            err(
                "data_asset_bad_path",
                "`path` (a .data.ron under assets/) is required",
            )
            .rpc(HINT)
        })?;
    let discard = match params.get("discard") {
        None | Some(Value::Null) => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| err("data_asset_bad_discard", "`discard` is a boolean").rpc(HINT))?,
    };
    // The panel's Revert: drop the open asset's unsaved edits, then (re)open the file. The new
    // file is read and parsed first, so a refused call (`data_asset_not_found`,
    // `data_asset_parse`, ...) keeps the edits (rev-claude-58 parity finding 1, ADR-7495).
    let dropped = if discard {
        let root = project_root_of(world).map_err(|e| e.rpc(HINT))?;
        let registry = registry_of(world).map_err(|e| e.rpc(HINT))?;
        let registry = registry.read();
        DataAssetDoc::open(&root, path, &registry).map_err(|e| e.rpc(HINT))?;
        world
            .get_resource_mut::<DataAssetEditor>()
            .and_then(|mut e| e.doc.take())
            .is_some_and(|doc| doc.dirty(&registry))
    } else {
        false
    };
    open_data_asset(world, path).map_err(|e| e.rpc(HINT))?;
    let mut out = doc_json(world)?;
    out["discarded"] = json!(dropped);
    Ok(out)
}

/// `editor.data_asset_set { field?, value }`.
fn data_asset_set_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let field = params.get("field").and_then(Value::as_str).unwrap_or("");
    let value = params
        .get("value")
        .ok_or_else(|| err("data_asset_bad_value", "`value` is required").rpc(HINT))?;
    let changed = edit_data_asset(world, field, value).map_err(|e| e.rpc(HINT))?;
    let mut out = doc_json(world)?;
    out["changed"] = json!(changed);
    Ok(out)
}

/// `editor.data_asset_save`.
fn data_asset_save_method(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let (path, bytes) = save_data_asset(world).map_err(|e| e.rpc(HINT))?;
    Ok(json!({ "path": path, "bytes": bytes }))
}

/// The data asset agent methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.data_asset_types",
                tool: "data_asset_types",
                description: "The registered data asset types the Content Browser's New ▸ Data asset list offers (a #[derive(Asset)] type registered with `register_data_asset` or #[reflect(Asset)]; the derive alone registers nothing): what a game can load. editor.create_asset { kind: \"data_asset\", type } takes any registered data asset type; anything else is refused (data_asset_unknown_type).",
                params: &[],
                result: "{ types: [type_path] }",
                mutating: false,
                permission: Permission::Read,
            },
            data_asset_types_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.data_asset_open",
                tool: "data_asset_open",
                description: "Open a .data.ron in the Data Asset panel. Refused (data_asset_unsaved) while the open asset has unsaved edits, unless discard: true drops them (the panel's Revert is discard: true with the open path).",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Project-relative path of a .data.ron under assets/.",
                    },
                    ParamSchema {
                        name: "discard",
                        kind: "boolean",
                        required: false,
                        description: "Drop the open asset's unsaved edits first (the panel's Revert); default false refuses with data_asset_unsaved.",
                    },
                ],
                result: "{ path, type, value, dirty, discarded }",
                mutating: true,
                permission: Permission::Assets,
            },
            data_asset_open_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.data_asset_set",
                tool: "data_asset_set",
                description: "Set a field of the open data asset, as the panel's widgets do: one undo step (editor.undo). The file is written by editor.data_asset_save.",
                params: &[
                    ParamSchema {
                        name: "field",
                        kind: "string",
                        required: false,
                        description: "Reflect path (`rolls`, `weights[1]`, `drop.chance`); empty or absent sets the whole value.",
                    },
                    ParamSchema {
                        name: "value",
                        kind: "any",
                        required: true,
                        description: "The new value in the reflect serializer's JSON form (as `value` in the result).",
                    },
                ],
                result: "{ path, type, value, dirty, changed }",
                mutating: true,
                permission: Permission::Assets,
            },
            data_asset_set_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.data_asset_save",
                tool: "data_asset_save",
                description: "Write the open data asset to its .data.ron (atomically).",
                params: &[],
                result: "{ path, bytes }",
                mutating: true,
                permission: Permission::Assets,
            },
            data_asset_save_method as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_app::prelude::{Reflect, ReflectDefault};

    /// A game-style data asset type: registered as one (`ReflectAsset` type data), so a game
    /// loads files of this type.
    #[derive(Reflect, Default, Clone, PartialEq, Debug)]
    #[reflect(Default)]
    struct LootTable {
        rolls: u32,
        weights: Vec<f32>,
        label: String,
    }
    impl regia_asset::data_asset::Asset for LootTable {}

    /// A registered `ReflectDefault` struct that is NOT a data asset: the editor must not offer
    /// or create it (a game could not load the file).
    #[derive(Reflect, Default, Clone, PartialEq, Debug)]
    #[reflect(Default)]
    struct WidgetTuning {
        factor: f32,
    }

    fn registry() -> TypeRegistry {
        let mut registry = TypeRegistry::default();
        registry.register::<LootTable>();
        registry.register_type_data::<LootTable, regia_asset::data_asset::ReflectAsset>();
        registry.register::<WidgetTuning>();
        registry.register::<u32>();
        registry.register::<f32>();
        registry.register::<String>();
        registry.register::<Vec<f32>>();
        registry
    }

    #[test]
    fn a_created_asset_opens_edits_saves_and_reopens() {
        let root = regia_test_utils::unique_temp_dir("data-asset");
        let reg = registry();
        let path = create_data_asset(&root, "loot/table.data.ron", "LootTable", &reg).expect("create");
        assert_eq!(path, "assets/loot/table.data.ron");
        let mut doc = DataAssetDoc::open(&root, &path, &reg).expect("open");
        assert!(doc.type_path.ends_with("LootTable"), "{}", doc.type_path);
        assert!(!doc.dirty(&reg));
        doc.set_field("rolls", &json!(3), &reg).expect("rolls");
        doc.set_field("label", &json!("line one\nline two"), &reg)
            .expect("label");
        doc.set_field("weights", &json!([1.0, 2.5]), &reg)
            .expect("weights");
        assert!(doc.dirty(&reg));
        doc.save(&root, &reg).expect("save");
        assert!(!doc.dirty(&reg));
        let again = DataAssetDoc::open(&root, &path, &reg).expect("reopen");
        let value = again
            .value()
            .downcast_ref::<LootTable>()
            .expect("LootTable")
            .clone();
        assert_eq!(
            value,
            LootTable {
                rolls: 3,
                weights: vec![1.0, 2.5],
                label: "line one\nline two".into(),
            }
        );
        // An unedited asset saves with the same bytes.
        let text = std::fs::read_to_string(root.join("assets/loot/table.data.ron")).expect("read");
        assert_eq!(again.to_text(&reg).expect("text"), text);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ADR-7310: the editor writes the runtime envelope, so the cook's header check (the
    /// `data_asset` importer) accepts what the editor creates, and the editor still opens a file
    /// in its first `type_path` spelling.
    #[test]
    fn the_editor_writes_the_runtime_envelope_and_reads_the_old_spelling() {
        let root = regia_test_utils::unique_temp_dir("data-asset-envelope");
        let reg = registry();
        let path = create_data_asset(&root, "env.data.ron", "LootTable", &reg).expect("create");
        let text = std::fs::read_to_string(root.join(&path)).expect("read");
        assert!(text.starts_with("DataAsset("), "{text}");
        assert!(text.contains("    type: \""), "{text}");
        let header = regia_asset::data_asset::DataAssetFile::header(&text).expect("runtime header");
        assert_eq!(header.format_version, DATA_ASSET_FORMAT_VERSION);
        assert!(header.type_path.ends_with("LootTable"), "{}", header.type_path);
        let doc = DataAssetDoc::open(&root, &path, &reg).expect("open");
        std::fs::write(
            root.join("assets/old.data.ron"),
            format!(
                "(format_version: 1, type_path: {:?}, value: (rolls: 2))",
                doc.type_path
            ),
        )
        .expect("write");
        let old = DataAssetDoc::open(&root, "old.data.ron", &reg).expect("old spelling");
        assert_eq!(
            old.value().downcast_ref::<LootTable>().expect("LootTable").rolls,
            2
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bad_paths_types_fields_and_versions_are_refused_with_codes() {
        let root = regia_test_utils::unique_temp_dir("data-asset-bad");
        let reg = registry();
        let code = |r: Result<String, PanelError>| r.expect_err("refused").code;
        assert_eq!(
            code(create_data_asset(&root, "x.ron", "LootTable", &reg)),
            "data_asset_bad_path"
        );
        assert_eq!(
            code(create_data_asset(&root, "../x.data.ron", "LootTable", &reg)),
            "data_asset_bad_path"
        );
        assert_eq!(
            code(create_data_asset(&root, "x.data.ron", "Nope", &reg)),
            "data_asset_unknown_type"
        );
        create_data_asset(&root, "x.data.ron", "LootTable", &reg).expect("create");
        assert_eq!(
            code(create_data_asset(&root, "x.data.ron", "LootTable", &reg)),
            "data_asset_exists"
        );
        let mut doc = DataAssetDoc::open(&root, "x.data.ron", &reg).expect("open");
        assert_eq!(
            doc.set_field("nope", &json!(1), &reg).expect_err("field").code,
            "data_asset_no_field"
        );
        assert_eq!(
            doc.set_field("rolls", &json!("three"), &reg)
                .expect_err("value")
                .code,
            "data_asset_bad_value"
        );
        assert!(!doc.dirty(&reg), "a refused edit leaves the value alone");
        std::fs::write(
            root.join("assets/new.data.ron"),
            "(format_version: 9, type_path: \"LootTable\", value: ())",
        )
        .expect("write");
        assert_eq!(
            DataAssetDoc::open(&root, "new.data.ron", &reg)
                .expect_err("version")
                .code,
            "data_asset_version"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Seeded byte mutations of a valid file (a fixed xorshift stream, no clock): parsing the file
    /// and its value never panics (`docs/formats/data-asset-ron.md`).
    #[test]
    fn mutated_data_asset_text_never_panics() {
        let reg = registry();
        let value = LootTable {
            rolls: 2,
            weights: vec![0.5, 1.5],
            label: "x".into(),
        };
        let text = file_text("LootTable", &value, &reg).expect("text");
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let alphabet = b"(),:[]\"0123456789.-abcxyz_ \n";
        for _ in 0..2_000 {
            let mut bytes = text.as_bytes().to_vec();
            for _ in 0..(1 + next() % 4) {
                let at = (next() as usize) % bytes.len();
                match next() % 3 {
                    0 => bytes[at] = alphabet[(next() as usize) % alphabet.len()],
                    1 => {
                        bytes.remove(at);
                    }
                    _ => bytes.insert(at, alphabet[(next() as usize) % alphabet.len()]),
                }
            }
            let Ok(mutated) = String::from_utf8(bytes) else {
                continue;
            };
            if let Ok(file) = DataAssetFile::from_ron_str(&mutated) {
                let _ = parse_value(&reg, &file.type_path, file.value.get_ron());
            }
        }
    }

    #[test]
    fn the_type_list_offers_registered_data_asset_types_only() {
        let mut reg = registry();
        reg.register::<regia_core::math::Vec3>();
        let types = data_asset_types(&reg);
        assert!(types.iter().any(|t| t.ends_with("LootTable")), "{types:?}");
        assert!(
            !types.iter().any(|t| t.ends_with("WidgetTuning")),
            "a registered ReflectDefault struct that is not a data asset is not offered: {types:?}"
        );
        assert!(!types.iter().any(|t| t.starts_with("glam::")), "{types:?}");
    }

    /// ADR-7310 (11): what the editor creates round-trips through the runtime loader
    /// (`DataAsset(format_version, type, value)`), and a type a game cannot load is refused
    /// with a known code.
    #[test]
    fn a_created_asset_round_trips_through_the_runtime_loader_and_unregistered_types_are_refused() {
        let root = regia_test_utils::unique_temp_dir("data-asset-round-trip");
        let reg = registry();
        let path = create_data_asset(&root, "loot/table.data.ron", "LootTable", &reg).expect("create");
        let text = std::fs::read_to_string(root.join(&path)).expect("read");
        assert!(text.starts_with("DataAsset("), "{text}");
        let loaded =
            regia_asset::data_asset::DataAssetFile::parse(&text, &reg).expect("the runtime loader reads it");
        let loot = loaded.value.downcast_ref::<LootTable>().expect("a LootTable");
        assert_eq!(*loot, LootTable::default());
        // A registered ReflectDefault struct that is not a data asset: refused, nothing written.
        let refused = create_data_asset(&root, "tuning/w.data.ron", "WidgetTuning", &reg)
            .expect_err("a game could not load the file");
        assert_eq!(refused.code, "data_asset_unknown_type");
        assert!(
            !root.join("assets/tuning").exists(),
            "the refused create writes nothing"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
