// SPDX-License-Identifier: MPL-2.0
//! Pinned fields, the Inspector lock and pinned Inspector windows (spec addendum § 7.2, § 7.3;
//! P-113, P-117).
//!
//! - **Pin to top** (a field label's context menu) pins a field for its component type; pinned
//!   fields draw in a **Pinned** section at the top of that component on every entity that has
//!   it. Pins are per user: `<user config>/regia/inspector_pins.ron` ([`PINS_FILE`], format
//!   version [`INSPECTOR_PINS_FORMAT_VERSION`]), written atomically on each change.
//! - The lock button keeps the Inspector on its entity while the selection changes
//!   ([`InspectorLock`]); the lock drops when the entity is despawned.
//! - **Open pinned inspector** opens a floating Inspector window locked to the inspected entity
//!   ([`PinnedInspectors`]); several may be open, and each closes when its entity is despawned.
//!   They float inside the editor window.
//!
//! None of this is on the undo stack or dirties a document (view state).

use std::collections::BTreeMap;
use std::path::Path;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_ecs::reflect::AppTypeRegistry;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::editor_prefs::EditorConfigPaths;
use crate::selection::Selection;

/// The pins file in the user config directory.
pub const PINS_FILE: &str = "inspector_pins.ron";

/// The current `inspector_pins.ron` format version (a missing field reads as 1).
pub const INSPECTOR_PINS_FORMAT_VERSION: u32 = 1;

/// `inspector_pins.ron` on disk.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinsFile {
    /// [`INSPECTOR_PINS_FORMAT_VERSION`].
    #[serde(default = "current_version")]
    pub format_version: u32,
    /// Pinned reflect paths per component type path, in pin order.
    #[serde(default)]
    pub pins: BTreeMap<String, Vec<String>>,
}

fn current_version() -> u32 {
    INSPECTOR_PINS_FORMAT_VERSION
}

/// Parses `inspector_pins.ron`.
///
/// # Errors
/// A RON error, or a file written by a newer editor (`format_version` above
/// [`INSPECTOR_PINS_FORMAT_VERSION`]).
pub fn parse_pins(text: &str) -> Result<PinsFile, String> {
    /// The version alone, read leniently first: a newer file may add fields this editor's
    /// `deny_unknown_fields` shape refuses, and it must be reported as newer, not as malformed.
    #[derive(Deserialize)]
    struct Version {
        #[serde(default = "current_version")]
        format_version: u32,
    }
    if let Ok(Version { format_version }) = ron::from_str::<Version>(text)
        && format_version > INSPECTOR_PINS_FORMAT_VERSION
    {
        return Err(format!(
            "written by a newer editor (format_version {format_version}, this editor reads up to {INSPECTOR_PINS_FORMAT_VERSION})"
        ));
    }
    ron::from_str(text).map_err(|e| e.to_string())
}

/// The `from` versions of the registered `inspector_pins.ron` migration steps (none: version 1 is
/// the first).
#[must_use]
pub fn pins_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde shape of `inspector_pins.ron` (ADR-7902: the `inspector-pins` format).
#[must_use]
pub fn pins_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[regia_core::schema::serde_field_names::<PinsFile>()])
}

/// `inspector_pins.ron`'s text for `pins`.
#[must_use]
pub fn pins_text(pins: &BTreeMap<String, Vec<String>>) -> String {
    let file = PinsFile {
        format_version: INSPECTOR_PINS_FORMAT_VERSION,
        pins: pins.clone(),
    };
    ron::ser::to_string_pretty(&file, regia_core::ron_pretty()).unwrap_or_default()
}

/// The user's pinned fields.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct InspectorPins {
    /// Pinned reflect paths per component type path, in pin order.
    pub pins: BTreeMap<String, Vec<String>>,
    /// Whether the file was read (or there is none to read) this session.
    pub loaded: bool,
    /// Why the pins file could not be read this session (malformed, written by a newer editor, or
    /// an IO error). While it is set no pin change is made or saved, so the file is left untouched
    /// (`pins_file_unreadable`).
    pub unreadable: Option<String>,
}

impl InspectorPins {
    /// The pinned paths of component type `type_path`.
    #[must_use]
    pub fn paths(&self, type_path: &str) -> &[String] {
        self.pins.get(type_path).map_or(&[], Vec::as_slice)
    }

    /// Whether `path` of `type_path` is pinned.
    #[must_use]
    pub fn is_pinned(&self, type_path: &str, path: &str) -> bool {
        self.paths(type_path).iter().any(|p| p == path)
    }

    /// Pins (`on`) or unpins `path` of `type_path`; returns whether anything changed.
    pub fn set(&mut self, type_path: &str, path: &str, on: bool) -> bool {
        if on {
            if self.is_pinned(type_path, path) {
                return false;
            }
            self.pins
                .entry(type_path.to_owned())
                .or_default()
                .push(path.to_owned());
            return true;
        }
        let Some(paths) = self.pins.get_mut(type_path) else {
            return false;
        };
        let before = paths.len();
        paths.retain(|p| p != path);
        let changed = paths.len() != before;
        if paths.is_empty() {
            self.pins.remove(type_path);
        }
        changed
    }
}

/// The pins file path, when the editor reads user files.
fn pins_path(world: &World) -> Option<std::path::PathBuf> {
    world
        .get_resource::<EditorConfigPaths>()
        .and_then(|paths| paths.dir.as_ref().map(|dir| dir.join(PINS_FILE)))
}

/// Reads the pins file once per session (a missing, unreadable or newer file leaves no pins and
/// is reported to the log).
pub fn ensure_pins_loaded(world: &mut World) {
    if world
        .get_resource::<InspectorPins>()
        .is_some_and(|pins| pins.loaded)
    {
        return;
    }
    let mut pins = InspectorPins {
        loaded: true,
        ..Default::default()
    };
    if let Some(path) = pins_path(world) {
        match std::fs::read_to_string(&path) {
            Ok(text) => match parse_pins(&text) {
                Ok(file) => pins.pins = file.pins,
                Err(err) => {
                    tracing::warn!(target: "regia_editor", "{}: {err}; left untouched", path.display());
                    pins.unreadable = Some(err);
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::warn!(target: "regia_editor", "{}: {err}; left untouched", path.display());
                pins.unreadable = Some(err.to_string());
            }
        }
    }
    world.insert_resource(pins);
}

fn save_pins(path: &Path, pins: &BTreeMap<String, Vec<String>>) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    crate::atomic_write::write_atomic(path, pins_text(pins).as_bytes())
}

/// The full type path of component `component` (a type path or a short name), when registered.
#[must_use]
pub fn full_type_path(world: &World, component: &str) -> Option<String> {
    let registry = world.get_resource::<AppTypeRegistry>()?.clone();
    let registry = registry.read();
    registry
        .get_with_type_path(component)
        .or_else(|| registry.get_with_short_type_path(component))
        .map(|registration| registration.type_info().type_path().to_owned())
}

/// Pins or unpins `path` of component `component` and writes the pins file. Returns whether the
/// pins changed.
///
/// # Errors
/// The pins file could not be read this session ([`InspectorPins::unreadable`]: nothing changes and
/// the file is left untouched, so its pins are not lost), or it could not be written (the pin
/// stays for the session).
pub fn set_pin(world: &mut World, component: &str, path: &str, on: bool) -> Result<bool, String> {
    ensure_pins_loaded(world);
    if let Some(err) = &world.resource::<InspectorPins>().unreadable {
        return Err(format!("{PINS_FILE} could not be read ({err}); left untouched"));
    }
    let type_path = full_type_path(world, component).unwrap_or_else(|| component.to_owned());
    let path = path.trim_start_matches('.');
    let changed = world.resource_mut::<InspectorPins>().set(&type_path, path, on);
    if changed && let Some(file) = pins_path(world) {
        let pins = world.resource::<InspectorPins>().pins.clone();
        save_pins(&file, &pins)?;
    }
    Ok(changed)
}

/// The Inspector lock: while `entity` is set (and alive) the Inspector shows it whatever the
/// selection is.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InspectorLock {
    /// The locked entity.
    pub entity: Option<Entity>,
}

/// The locked entity, when the lock is on and the entity is alive; a dead entity's lock is
/// dropped.
pub fn locked_entity(world: &mut World) -> Option<Entity> {
    let entity = world.get_resource::<InspectorLock>()?.entity?;
    if world.get_entity(entity).is_ok() {
        return Some(entity);
    }
    world.insert_resource(InspectorLock::default());
    None
}

/// Locks the Inspector on `entity` (`Some`) or unlocks it.
pub fn set_lock(world: &mut World, entity: Option<Entity>) {
    world.insert_resource(InspectorLock { entity });
}

/// The open pinned Inspector windows, by entity, in open order.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct PinnedInspectors {
    /// One window per entity.
    pub windows: Vec<Entity>,
}

/// Opens a pinned Inspector window on `entity`; returns `false` when one is already open.
pub fn open_pinned(world: &mut World, entity: Entity) -> bool {
    let mut pinned = world.get_resource_or_insert_with(PinnedInspectors::default);
    if pinned.windows.contains(&entity) {
        return false;
    }
    pinned.windows.push(entity);
    true
}

/// Closes the windows of despawned entities; returns the windows left.
pub fn live_pinned(world: &mut World) -> Vec<Entity> {
    let Some(pinned) = world.get_resource::<PinnedInspectors>() else {
        return Vec::new();
    };
    let live: Vec<Entity> = pinned
        .windows
        .iter()
        .copied()
        .filter(|entity| world.get_entity(*entity).is_ok())
        .collect();
    if live.len() != pinned.windows.len() {
        world.insert_resource(PinnedInspectors {
            windows: live.clone(),
        });
    }
    live
}

/// Draws the pinned Inspector windows (floating in the editor window).
pub fn draw_pinned_windows(ctx: &egui::Context, world: &mut World) {
    let windows = live_pinned(world);
    let mut closed = Vec::new();
    for entity in windows {
        let name = super::entity_label(world, entity);
        let mut open = true;
        egui::Window::new(format!("Inspector: {name}"))
            .id(egui::Id::new(("pinned_inspector", entity.to_bits())))
            .open(&mut open)
            .default_width(320.0)
            .vscroll(true)
            .show(ctx, |ui| {
                super::draw_entity(ui, world, entity, true);
            });
        if !open {
            closed.push(entity);
        }
    }
    if !closed.is_empty()
        && let Some(mut pinned) = world.get_resource_mut::<PinnedInspectors>()
    {
        pinned.windows.retain(|entity| !closed.contains(entity));
    }
}

/// The options a pinned field is drawn with: its own attributes, read from its parent struct.
fn pinned_field_options(
    component: &str,
    value: &dyn regia_ecs::reflect::PartialReflect,
    path: &str,
) -> super::options::FieldOptions {
    use regia_ecs::reflect::ReflectPath as _;
    let (parent, name) = path.rsplit_once('.').unwrap_or(("", path));
    let parent_value = if parent.is_empty() {
        Some(value)
    } else {
        parent.reflect_element(value).ok()
    };
    let attrs = parent_value
        .and_then(|parent| match parent.get_represented_type_info() {
            Some(regia_ecs::reflect::TypeInfo::Struct(info)) => info
                .field(name)
                .map(|field| regia_ecs::attrs::EditorAttrs::from_custom(field.custom_attributes())),
            _ => None,
        })
        .unwrap_or_default();
    super::options::FieldOptions::for_field(component, name, attrs)
}

/// Draws the **Pinned** section of a component: each pinned field of `value` (that the row
/// filter keeps) with its widget, edits recorded at its own path. Its label's context menu
/// unpins it (the pick lands in `context`'s pin requests).
pub(crate) fn pinned_section(
    ui: &mut egui::Ui,
    registry: &regia_ecs::reflect::TypeRegistry,
    context: &mut super::InspectorContext,
    value: &mut dyn regia_ecs::reflect::PartialReflect,
    component: &str,
    pins: &[String],
) {
    use regia_ecs::reflect::ReflectPath as _;
    let visible: Vec<&String> = pins.iter().filter(|path| context.row_visible(path)).collect();
    if visible.is_empty() {
        return;
    }
    let id = egui::Id::new(("inspector_pinned", component));
    egui::CollapsingHeader::new("Pinned")
        .id_salt(id)
        .default_open(true)
        .show(ui, |ui| {
            egui::Grid::new(id.with("grid")).num_columns(2).show(ui, |ui| {
                for path in visible {
                    let options = pinned_field_options(component, value, path);
                    let Ok(field) = path.as_str().reflect_element_mut(value) else {
                        continue;
                    };
                    let label = path.rsplit('.').next().unwrap_or(path);
                    let shown = options.attrs.display_name.unwrap_or(label);
                    let response = ui.add(egui::Label::new(shown).sense(egui::Sense::click()));
                    response.context_menu(|ui| {
                        if crate::agent_parity::action_button_text(
                            ui,
                            &crate::agent_parity::INSPECTOR_PIN_FIELD,
                            "Unpin from top",
                        )
                        .clicked()
                        {
                            context.request_pin(path, false);
                            ui.close();
                        }
                    });
                    super::InspectorUi::new(registry, context).ui_at_path(
                        field,
                        path,
                        ui,
                        id.with(path.as_str()),
                        &options,
                    );
                    ui.end_row();
                }
            });
        });
}

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

/// An error with a lowercase `reason` code in its data.
fn reason(mut err: RpcError, code: &str) -> RpcError {
    match err.data.as_mut() {
        Some(Value::Object(data)) => {
            data.insert("reason".into(), Value::String(code.to_owned()));
        }
        _ => err.data = Some(json!({ "reason": code })),
    }
    err
}

/// One entity id parameter.
fn entity_param(world: &World, params: &Value, name: &str) -> Result<Entity, RpcError> {
    let Some(bits) = params.get(name).and_then(Value::as_u64) else {
        return Err(reason(
            RpcError::invalid_params(format!("`{name}` must be an entity id")),
            "entity_required",
        ));
    };
    crate::command::resolve_entity(world, bits).ok_or_else(|| {
        reason(
            RpcError::not_found(
                format!("entity {bits} does not exist"),
                "use scene.query_entities",
            ),
            "entity_gone",
        )
    })
}

/// The `entities` parameter (absent: the selection).
pub(crate) fn entities_param(world: &mut World, params: &Value) -> Result<Vec<Entity>, RpcError> {
    let Some(value) = params.get("entities").filter(|v| !v.is_null()) else {
        return Ok(world
            .get_resource_or_insert_with(Selection::default)
            .entities()
            .to_vec());
    };
    let Some(items) = value.as_array() else {
        return Err(RpcError::invalid_params(
            "`entities` must be an array of entity ids",
        ));
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let bits = item
            .as_u64()
            .ok_or_else(|| RpcError::invalid_params("`entities` elements must be entity ids"))?;
        let entity = crate::command::resolve_entity(world, bits).ok_or_else(|| {
            reason(
                RpcError::not_found(
                    format!("entity {bits} does not exist"),
                    "use scene.query_entities",
                ),
                "entity_gone",
            )
        })?;
        out.push(entity);
    }
    Ok(out)
}

/// The agent surface: `editor.inspector_pin`, `editor.inspector_lock`,
/// `editor.inspector_open_pinned` (`book/agents/inspector.md`).
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.inspector_pin",
                tool: "inspector_pin",
                description: "Pins (on: true) or unpins a field for a component type, as the \
                    Inspector's Pin to top does: pinned fields show in a Pinned section at the top \
                    of that component for every entity with it. Pins are per user \
                    (inspector_pins.ron in the user config directory); not undoable.",
                params: &[
                    ParamSchema {
                        name: "type",
                        kind: "string",
                        required: true,
                        description: "Component type path or short name.",
                    },
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Reflect path of the field (intensity, translation.x).",
                    },
                    ParamSchema {
                        name: "on",
                        kind: "boolean",
                        required: false,
                        description: "true pins (default), false unpins.",
                    },
                ],
                result: "{ type, path, pinned, changed, pins: [path] }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_inspector_pin as Handler,
        ),
        (
            MethodSchema {
                name: "editor.inspector_lock",
                tool: "inspector_lock",
                description: "Locks the Inspector on an entity (on: true; the selection's primary \
                    entity unless `entity` is given) so it stays there while the selection \
                    changes, or unlocks it (on: false). The lock drops when the entity is \
                    despawned. View state: not undoable.",
                params: &[
                    ParamSchema {
                        name: "on",
                        kind: "boolean",
                        required: true,
                        description: "Lock or unlock.",
                    },
                    ParamSchema {
                        name: "entity",
                        kind: "integer",
                        required: false,
                        description: "The entity to lock on (default: the selection's primary).",
                    },
                ],
                result: "{ locked, entity }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_inspector_lock as Handler,
        ),
        (
            MethodSchema {
                name: "editor.inspector_open_pinned",
                tool: "inspector_open_pinned",
                description: "Opens a floating Inspector window locked to `entity`, as the \
                    Inspector's Open pinned inspector does. Several windows may be open; each \
                    closes when its entity is despawned. close: true closes the entity's window.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "integer",
                        required: true,
                        description: "The entity the window shows.",
                    },
                    ParamSchema {
                        name: "close",
                        kind: "boolean",
                        required: false,
                        description: "Close the window instead (default false).",
                    },
                ],
                result: "{ entity, open, opened, windows: [entity] }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_inspector_open_pinned as Handler,
        ),
    ]
}

fn bool_param(params: &Value, name: &str, default: bool) -> Result<bool, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(on)) => Ok(*on),
        Some(_) => Err(RpcError::invalid_params(format!("`{name}` must be a boolean"))),
    }
}

/// `editor.inspector_pin`.
fn editor_inspector_pin(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(component) = params.get("type").and_then(Value::as_str) else {
        return Err(reason(
            RpcError::invalid_params("`type` must be a string"),
            "type_required",
        ));
    };
    let Some(path) = params.get("path").and_then(Value::as_str) else {
        return Err(reason(
            RpcError::invalid_params("`path` must be a string"),
            "path_required",
        ));
    };
    let on = bool_param(params, "on", true)?;
    ensure_pins_loaded(world);
    if let Some(err) = world.resource::<InspectorPins>().unreadable.clone() {
        return Err(reason(
            RpcError::internal(format!(
                "{PINS_FILE} could not be read ({err}); it is left untouched and no pin is changed this session"
            )),
            "pins_file_unreadable",
        ));
    }
    let Some(type_path) = full_type_path(world, component) else {
        return Err(reason(
            RpcError::not_found(
                format!("no component type {component}"),
                "see editor.component_types",
            ),
            "type_unknown",
        ));
    };
    let path = path.trim_start_matches('.');
    if path.is_empty() {
        return Err(reason(
            RpcError::invalid_params("`path` must name a field"),
            "path_required",
        ));
    }
    let changed = set_pin(world, &type_path, path, on).map_err(|err| {
        reason(
            RpcError::internal(format!("writing {PINS_FILE}: {err}")),
            "pins_write_failed",
        )
    })?;
    let pins = world.resource::<InspectorPins>().paths(&type_path).to_vec();
    Ok(json!({
        "type": type_path,
        "path": path,
        "pinned": pins.iter().any(|p| p == path),
        "changed": changed,
        "pins": pins,
    }))
}

/// `editor.inspector_lock`.
fn editor_inspector_lock(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(on) = params.get("on").and_then(Value::as_bool) else {
        return Err(reason(
            RpcError::invalid_params("`on` must be a boolean"),
            "on_required",
        ));
    };
    if !on {
        set_lock(world, None);
        return Ok(json!({ "locked": false, "entity": Value::Null }));
    }
    let entity = if params.get("entity").is_some_and(|v| !v.is_null()) {
        entity_param(world, params, "entity")?
    } else {
        let primary = world.get_resource_or_insert_with(Selection::default).primary();
        primary.ok_or_else(|| {
            reason(
                RpcError::invalid_params("nothing is selected; pass `entity`"),
                "nothing_selected",
            )
        })?
    };
    set_lock(world, Some(entity));
    Ok(json!({ "locked": true, "entity": entity.to_bits() }))
}

/// `editor.inspector_open_pinned`.
fn editor_inspector_open_pinned(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_param(world, params, "entity")?;
    let close = bool_param(params, "close", false)?;
    let opened = if close {
        if let Some(mut pinned) = world.get_resource_mut::<PinnedInspectors>() {
            pinned.windows.retain(|e| *e != entity);
        }
        false
    } else {
        open_pinned(world, entity)
    };
    let windows = live_pinned(world);
    Ok(json!({
        "entity": entity.to_bits(),
        "open": windows.contains(&entity),
        "opened": opened,
        "windows": windows.iter().map(|e| e.to_bits()).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_round_trip_and_a_newer_file_is_refused() {
        let mut pins = InspectorPins::default();
        assert!(pins.set("a::Lamp", "intensity", true));
        assert!(
            !pins.set("a::Lamp", "intensity", true),
            "pinning twice changes nothing"
        );
        assert!(pins.set("a::Lamp", "color", true));
        let text = pins_text(&pins.pins);
        let back = parse_pins(&text).expect("parse");
        assert_eq!(back.format_version, INSPECTOR_PINS_FORMAT_VERSION);
        assert_eq!(back.pins["a::Lamp"], ["intensity", "color"]);
        assert!(parse_pins("(pins: {})").is_ok(), "a missing version reads as 1");
        let newer = parse_pins("(format_version: 2, pins: {})").expect_err("newer");
        assert!(newer.contains("newer editor"), "{newer}");
        assert!(pins.set("a::Lamp", "intensity", false));
        assert!(pins.set("a::Lamp", "color", false));
        assert!(pins.pins.is_empty(), "the last unpin drops the type");
        let newer_shape = parse_pins("(format_version: 2, pins: {}, groups: [])").expect_err("newer");
        assert!(
            newer_shape.contains("newer editor"),
            "a newer file with a new field is newer, not malformed: {newer_shape}"
        );
    }

    /// Review finding 6 of ADR-7758: an unreadable `inspector_pins.ron` (malformed, or from a newer
    /// editor) is left untouched. A pin is refused (`pins_file_unreadable` from the agent), the file
    /// keeps its bytes, and no pin is set for the session.
    #[test]
    fn an_unreadable_pins_file_is_left_untouched_and_pinning_is_refused() {
        for text in [
            "(pins: {\"a::Lamp\": [\"color\"",
            "(format_version: 9, pins: {}, groups: [])",
        ] {
            let dir = regia_test_utils::unique_temp_dir("inspector-pins-unreadable");
            std::fs::write(dir.join(PINS_FILE), text).expect("write");
            let mut world = World::new();
            world.insert_resource(EditorConfigPaths {
                dir: Some(dir.clone()),
            });
            let err = set_pin(&mut world, "a::Lamp", "intensity", true).expect_err("refused");
            assert!(err.contains("left untouched"), "{err}");
            assert!(world.resource::<InspectorPins>().unreadable.is_some());
            assert!(world.resource::<InspectorPins>().pins.is_empty(), "no pin set");
            let err = editor_inspector_pin(&mut world, &json!({ "type": "a::Lamp", "path": "intensity" }))
                .expect_err("the agent is refused too");
            assert_eq!(
                err.data.as_ref().and_then(|d| d["reason"].as_str()),
                Some("pins_file_unreadable")
            );
            assert_eq!(
                std::fs::read_to_string(dir.join(PINS_FILE)).expect("read"),
                text,
                "the file keeps its bytes"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
