// SPDX-License-Identifier: MPL-2.0
//! Inspector field tools (addendum § 7.4-7.6; P-114, P-115, P-120, P-122): copy and paste of a
//! value or a whole component, reset to default, the raw view, and list moves and duplicates.
//!
//! Every edit here is one `SetProperty` (or `AddComponent`) on the undo stack, sealed on both
//! sides, so each paste, reset or list move is exactly one undo step. The Inspector's menus and
//! the agent methods (`editor.property_copy`, `editor.property_paste`, `editor.component_copy`,
//! `editor.component_paste`, `editor.reset_property`, `editor.list_move`,
//! `editor.list_duplicate`) call the same functions.
//!
//! The clipboard ([`InspectorClipboard`]) holds the copied value as a typed reflect value plus
//! its RON text: a paste works on any entity with the same field or component type, for the
//! whole editor session, and refuses a different type (`type_mismatch`) with a notice in the
//! Inspector.

use std::collections::BTreeSet;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, PartialReflect, ReflectMut, ReflectPath, TypeRegistry};
use serde_json::{Value, json};

use super::reflect_ui::{ListOp, apply_list_op, clone_concrete};
use crate::command::{AddComponent, EditorCommand, SetProperty, UndoStack};

/// An agent method handler.
type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

/// How long an Inspector notice stays up, seconds.
pub const NOTICE_SECONDS: f64 = 4.0;

/// The Inspector's clipboard and its last notice.
#[derive(Resource, Default)]
pub struct InspectorClipboard {
    /// The copied value, if any.
    pub entry: Option<ClipEntry>,
    /// The last copy/paste/reset message and whether it is a refusal; shown at the top of the
    /// Inspector for [`NOTICE_SECONDS`].
    pub notice: Option<Notice>,
}

/// A transient Inspector message (the "toast" of a copy, paste or refusal).
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    /// The message.
    pub text: String,
    /// A refusal (drawn in the theme's warning color).
    pub error: bool,
    /// egui time when it was first drawn.
    pub shown_at: Option<f64>,
}

/// A copied value.
#[derive(Debug)]
pub struct ClipEntry {
    /// Type path of the value (`f32`, `regia_render::lights::PointLight`).
    pub type_path: String,
    /// The value as RON.
    pub ron: String,
    /// The value.
    pub value: Box<dyn PartialReflect>,
    /// Where it was copied from: `(component type path, reflect path)`; `""` = the whole
    /// component.
    pub source: (String, String),
}

impl ClipEntry {
    /// Whether this is a whole component.
    #[must_use]
    pub fn is_component(&self) -> bool {
        self.source.1.is_empty()
    }
}

/// Whether the Inspector's raw view ([`InspectorRawView`]) is on.
#[must_use]
pub fn raw_view(world: &World) -> bool {
    world.get_resource::<InspectorRawView>().is_some_and(|raw| raw.0)
}

/// The Inspector's raw view (P-120): hidden fields and structural components shown read-only.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InspectorRawView(pub bool);

/// A field context-menu pick or field shortcut (`reflect_ui::InspectorContext` collects them while
/// drawing; [`run_field_requests`] carries them out after).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldRequest {
    /// Copy value.
    Copy(String),
    /// Paste value.
    Paste(String),
    /// Copy as RON (system clipboard).
    CopyRon(String),
    /// Copy agent path (system clipboard).
    CopyAgentPath(String),
    /// Reset to default.
    Reset(String),
}

/// What the field drawer needs to know about a component: its entity, its baseline (the
/// default or the prefab's value), the fields that differ from it, and the raw view.
#[derive(Debug, Default)]
pub struct FieldTools {
    /// The entity's id.
    pub entity: u64,
    /// The value fields are compared to and reset to (`inspector::defaults::baseline`).
    pub baseline: Option<Box<dyn PartialReflect>>,
    /// `"default"` or `"prefab"`.
    pub baseline_source: &'static str,
    /// Reflect paths of the leaves that differ from the baseline.
    pub modified: BTreeSet<String>,
    /// The raw view is on.
    pub raw: bool,
}

impl FieldTools {
    /// The tools for `entity`'s component `component` whose current value is `value`.
    #[must_use]
    pub fn for_component(world: &World, entity: Entity, component: &str, value: &dyn PartialReflect) -> Self {
        let _span = tracing::trace_span!(target: "regia_editor", "inspector_field_tools").entered();
        let baseline = super::defaults::baseline(world, entity, component);
        let (baseline_source, baseline) = match baseline {
            Some((super::defaults::BaselineSource::Prefab, value)) => ("prefab", Some(value)),
            Some((super::defaults::BaselineSource::TypeDefault, value)) => ("default", Some(value)),
            None => ("default", None),
        };
        let modified = baseline
            .as_deref()
            .map(|baseline| super::defaults::modified_paths(value, baseline))
            .unwrap_or_default();
        Self {
            entity: entity.to_bits(),
            baseline,
            baseline_source,
            modified,
            raw: raw_view(world),
        }
    }

    /// Whether the field at `path` (or a field inside it) differs from the baseline.
    #[must_use]
    pub fn is_modified(&self, path: &str) -> bool {
        if self.modified.contains(path) {
            return true;
        }
        self.modified
            .range(path.to_owned()..)
            .take_while(|p| p.starts_with(path))
            .any(|p| matches!(p.as_bytes().get(path.len()), Some(b'.' | b'[')))
    }

    /// The baseline's value at `path` as compact RON (the tooltip's default line).
    #[must_use]
    pub fn baseline_ron(&self, registry: &TypeRegistry, path: &str) -> Option<String> {
        let baseline = self.baseline.as_deref()?;
        let value = if path.is_empty() {
            baseline
        } else {
            path.reflect_element(baseline).ok()?
        };
        to_ron(value, registry)
    }
}

/// Why a clipboard, reset or list operation was refused: an agent `reason` code and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipError {
    /// Lowercase reason code (`clipboard_empty`, `type_mismatch`, ...), listed on the agent page.
    pub code: &'static str,
    /// What went wrong, for people.
    pub message: String,
}

impl ClipError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ClipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// The type path a value represents (a dynamic value reports its represented type).
#[must_use]
pub fn type_path_of(value: &dyn PartialReflect) -> String {
    value
        .get_represented_type_info()
        .map(|info| info.type_path().to_owned())
        .unwrap_or_else(|| value.reflect_type_path().to_owned())
}

/// `value` as compact RON through the type registry.
#[must_use]
pub fn to_ron(value: &dyn PartialReflect, registry: &TypeRegistry) -> Option<String> {
    let serializer = regia_ecs::reflect::serde::TypedReflectSerializer::new(value, registry);
    ron::ser::to_string(&serializer).ok()
}

fn to_json(value: &dyn PartialReflect, registry: &TypeRegistry) -> Value {
    let serializer = regia_ecs::reflect::serde::TypedReflectSerializer::new(value, registry);
    serde_json::to_value(&serializer).unwrap_or(Value::Null)
}

/// `(full type path, short type path)` of a registered component name (full or short).
fn component_names(world: &World, component: &str) -> Result<(String, String), ClipError> {
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    registry
        .get_with_type_path(component)
        .or_else(|| registry.get_with_short_type_path(component))
        .or_else(|| regia_core::brand::lookup_type_path(component, |p| registry.get_with_type_path(p)))
        .map(|registration| {
            let info = registration.type_info();
            (
                info.type_path().to_owned(),
                info.type_path_table().short_path().to_owned(),
            )
        })
        .ok_or_else(|| ClipError::new("component_unknown", format!("no component type {component}")))
}

/// Reads `entity`'s `component` at `path` as a concrete clone.
fn read_value(
    world: &World,
    entity: Entity,
    component: &str,
    path: &str,
) -> Result<Box<dyn PartialReflect>, ClipError> {
    let value = crate::command::read_property(world, entity, component, path).map_err(|err| {
        let code = if path.is_empty() {
            "component_missing"
        } else {
            "path_not_found"
        };
        ClipError::new(code, err.to_string())
    })?;
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    Ok(clone_concrete(&registry, value.as_ref()).unwrap_or(value))
}

/// Applies `command`, pushes it on the undo stack as its own step.
fn push_step(world: &mut World, mut command: impl EditorCommand) {
    command.apply(world);
    let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    stack.seal();
    stack.push(Box::new(command));
    stack.seal();
}

/// Writes `new` at `path` of `entity`'s `component` as one undo step.
fn write_step(
    world: &mut World,
    entity: Entity,
    component: &str,
    path: &str,
    new: Box<dyn PartialReflect>,
) -> Result<(), ClipError> {
    let mut command = SetProperty::capture(world, entity, component, path, new)
        .map_err(|err| ClipError::new("path_not_found", err.to_string()))?;
    command.coalesce = false;
    push_step(world, command);
    Ok(())
}

/// Copies `entity`'s `component` at `path` (`""` = the whole component) to the clipboard.
///
/// # Errors
/// `component_unknown`, `component_missing`, `path_not_found`, `not_serializable`.
pub fn copy_property(
    world: &mut World,
    entity: Entity,
    component: &str,
    path: &str,
) -> Result<(), ClipError> {
    let (full, _) = component_names(world, component)?;
    let value = read_value(world, entity, &full, path)?;
    let registry = world.resource::<AppTypeRegistry>().clone();
    let ron = to_ron(value.as_ref(), &registry.read()).ok_or_else(|| {
        ClipError::new(
            "not_serializable",
            format!("{} has no serialized form", type_path_of(value.as_ref())),
        )
    })?;
    let entry = ClipEntry {
        type_path: type_path_of(value.as_ref()),
        ron,
        value,
        source: (full, path.to_owned()),
    };
    world
        .get_resource_or_insert_with(InspectorClipboard::default)
        .entry = Some(entry);
    Ok(())
}

/// A clone of the clipboard's value, if its type is `type_path`.
fn clip_value(world: &World, type_path: &str) -> Result<Box<dyn PartialReflect>, ClipError> {
    let Some(entry) = world
        .get_resource::<InspectorClipboard>()
        .and_then(|clip| clip.entry.as_ref())
    else {
        return Err(ClipError::new("clipboard_empty", "nothing has been copied"));
    };
    if entry.type_path != type_path {
        return Err(ClipError::new(
            "type_mismatch",
            format!("the clipboard holds {}, not {type_path}", entry.type_path),
        ));
    }
    let registry = world.resource::<AppTypeRegistry>().clone();
    clone_concrete(&registry.read(), entry.value.as_ref())
        .ok_or_else(|| ClipError::new("not_serializable", "the copied value cannot be cloned"))
}

/// Pastes the clipboard into `entity`'s `component` at `path` (`""` = the whole component) as one
/// undo step. The clipboard's type must be the field's.
///
/// # Errors
/// `clipboard_empty`, `type_mismatch`, `component_unknown`, `component_missing`, `path_not_found`.
pub fn paste_property(
    world: &mut World,
    entity: Entity,
    component: &str,
    path: &str,
) -> Result<(), ClipError> {
    let (full, _) = component_names(world, component)?;
    let value = paste_value(world, entity, &full, path)?;
    write_step(world, entity, &full, path, value)
}

/// The clipboard's value for `entity`'s `full` at `path`, type-checked.
fn paste_value(
    world: &World,
    entity: Entity,
    full: &str,
    path: &str,
) -> Result<Box<dyn PartialReflect>, ClipError> {
    let current = read_value(world, entity, full, path)?;
    clip_value(world, &type_path_of(current.as_ref()))
}

/// Pastes a copied component: onto `entity`'s component of the same type (`as_new: false`), or
/// as a new component of an entity that does not have it (`as_new: true`). One undo step.
///
/// # Errors
/// `clipboard_empty`, `not_a_component`, `component_missing`, `component_present`.
pub fn paste_component(world: &mut World, entity: Entity, as_new: bool) -> Result<String, ClipError> {
    let Some((type_path, is_component)) = world
        .get_resource::<InspectorClipboard>()
        .and_then(|clip| clip.entry.as_ref())
        .map(|entry| (entry.type_path.clone(), entry.is_component()))
    else {
        return Err(ClipError::new("clipboard_empty", "nothing has been copied"));
    };
    if !is_component {
        return Err(ClipError::new(
            "not_a_component",
            "the clipboard holds a field value; copy a whole component",
        ));
    }
    let (full, short) = component_names(world, &type_path)?;
    let present = crate::command::read_property(world, entity, &full, "").is_ok();
    let value = clip_value(world, &full)?;
    match (as_new, present) {
        (true, true) => Err(ClipError::new(
            "component_present",
            format!("the entity already has {short}; paste its values instead"),
        )),
        (false, false) => Err(ClipError::new(
            "component_missing",
            format!("the entity has no {short}; paste it as a new component"),
        )),
        (true, false) => {
            let command = AddComponent::capture(world, entity, &short, value)
                .map_err(|err| ClipError::new("component_unknown", err.to_string()))?;
            push_step(world, command);
            Ok(full)
        }
        (false, true) => write_step(world, entity, &full, "", value).map(|()| full),
    }
}

/// Resets `entity`'s `component` at `path` (`None` = every field) to its baseline: the prefab's
/// value on a prefab instance, else the type's `Default`. One undo step.
///
/// # Errors
/// `component_unknown`, `component_missing`, `no_default`, `path_not_found`.
pub fn reset_property(
    world: &mut World,
    entity: Entity,
    component: &str,
    path: Option<&str>,
) -> Result<(), ClipError> {
    let (full, _) = component_names(world, component)?;
    let value = reset_value(world, entity, &full, path.unwrap_or(""))?;
    write_step(world, entity, &full, path.unwrap_or(""), value)
}

/// The baseline value of `entity`'s `full` at `path` (`""` = the whole component).
fn reset_value(
    world: &World,
    entity: Entity,
    full: &str,
    path: &str,
) -> Result<Box<dyn PartialReflect>, ClipError> {
    let short = full.rsplit("::").next().unwrap_or(full);
    read_value(world, entity, full, "")?;
    let Some((_, baseline)) = super::defaults::baseline(world, entity, full) else {
        return Err(ClipError::new(
            "no_default",
            format!("{short} has no #[reflect(Default)] and no prefab value"),
        ));
    };
    let value = if path.is_empty() {
        Some(baseline)
    } else {
        path.reflect_element(baseline.as_ref())
            .ok()
            .and_then(super::reflect_ui::clone_value)
    };
    let value =
        value.ok_or_else(|| ClipError::new("path_not_found", format!("{short} has no field {path}")))?;
    let registry = world.resource::<AppTypeRegistry>().clone();
    let value = clone_concrete(&registry.read(), value.as_ref()).unwrap_or(value);
    Ok(value)
}

/// Applies a list operation to the list at `path` of `entity`'s `component` as one undo step;
/// returns the list's new length.
///
/// # Errors
/// `component_unknown`, `component_missing`, `path_not_found`, `not_a_list`, `index_out_of_range`.
pub fn list_edit(
    world: &mut World,
    entity: Entity,
    component: &str,
    path: &str,
    op: ListOp,
) -> Result<usize, ClipError> {
    let (full, _) = component_names(world, component)?;
    let mut list = read_value(world, entity, &full, path)?;
    let registry = world.resource::<AppTypeRegistry>().clone();
    let len = {
        let ReflectMut::List(items) = list.reflect_mut() else {
            return Err(ClipError::new("not_a_list", format!("{path} is not a list")));
        };
        apply_list_op(&registry.read(), items, op)
            .map_err(|err| ClipError::new("index_out_of_range", err))?;
        items.len()
    };
    write_step(world, entity, &full, path, list)?;
    Ok(len)
}

/// Sets the Inspector notice.
pub fn notify(world: &mut World, text: impl Into<String>, error: bool) {
    world
        .get_resource_or_insert_with(InspectorClipboard::default)
        .notice = Some(Notice {
        text: text.into(),
        error,
        shown_at: None,
    });
}

/// Carries out the field picks of one component draw (`ctx` for the system clipboard).
pub fn run_field_requests(
    ctx: &egui::Context,
    world: &mut World,
    entity: Entity,
    component: &str,
    requests: Vec<FieldRequest>,
) {
    for request in requests {
        let result = match &request {
            FieldRequest::Copy(path) => {
                copy_property(world, entity, component, path).map(|()| format!("Copied {path}"))
            }
            FieldRequest::Paste(path) => {
                paste_property(world, entity, component, path).map(|()| format!("Pasted into {path}"))
            }
            FieldRequest::Reset(path) => {
                reset_property(world, entity, component, Some(path)).map(|()| format!("Reset {path}"))
            }
            FieldRequest::CopyRon(path) => read_value(world, entity, component, path).and_then(|value| {
                let registry = world.resource::<AppTypeRegistry>().clone();
                let ron = to_ron(value.as_ref(), &registry.read())
                    .ok_or_else(|| ClipError::new("not_serializable", "no serialized form"))?;
                ctx.copy_text(ron);
                Ok(format!("Copied {path} as RON"))
            }),
            FieldRequest::CopyAgentPath(path) => {
                let call = agent_call(world, entity, component, path);
                ctx.copy_text(call.to_string());
                Ok(format!("Copied the agent path of {path}"))
            }
        };
        match result {
            Ok(text) => notify(world, text, false),
            Err(err) => notify(world, err.message, true),
        }
    }
}

/// Carries out the field picks of the multi-selection Inspector: copies read the first holder;
/// a paste or reset writes every holder of `component` as one compound undo step (a holder whose
/// field refuses the value is skipped and named in the notice).
pub fn run_multi_requests(
    ctx: &egui::Context,
    world: &mut World,
    holders: &[Entity],
    component: &str,
    requests: Vec<FieldRequest>,
) {
    let Some(&first) = holders.first() else {
        return;
    };
    for request in requests {
        let (path, paste) = match &request {
            FieldRequest::Paste(path) => (path.clone(), true),
            FieldRequest::Reset(path) => (path.clone(), false),
            _ => {
                run_field_requests(ctx, world, first, component, vec![request]);
                continue;
            }
        };
        let label = if paste { "Paste" } else { "Reset" };
        let mut group = crate::command::CommandGroup::new(format!("{label} {path}"));
        let mut refused = None;
        for &entity in holders {
            let value = if paste {
                paste_value(world, entity, component, &path)
            } else {
                reset_value(world, entity, component, &path)
            };
            let command = value.and_then(|value| {
                SetProperty::capture(world, entity, component, &path, value)
                    .map_err(|err| ClipError::new("path_not_found", err.to_string()))
            });
            match command {
                Ok(mut command) => {
                    command.coalesce = false;
                    command.apply(world);
                    group.push(Box::new(command));
                }
                Err(err) => refused = Some(err),
            }
        }
        if !group.commands.is_empty() {
            let count = group.commands.len();
            let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
            stack.seal();
            stack.push(Box::new(group));
            stack.seal();
            let done = if paste { "Pasted into" } else { "Reset" };
            notify(world, format!("{done} {path} on {count} entities"), false);
        }
        if let Some(err) = refused {
            notify(world, err.message, true);
        }
    }
}

/// The `editor.set_property` call that writes `path` of `entity`'s `component` with its current
/// value (Copy agent path).
#[must_use]
pub fn agent_call(world: &World, entity: Entity, component: &str, path: &str) -> Value {
    let value = read_value(world, entity, component, path)
        .map(|value| {
            let registry = world.resource::<AppTypeRegistry>().clone();
            to_json(value.as_ref(), &registry.read())
        })
        .unwrap_or(Value::Null);
    let component =
        component_names(world, component).map_or_else(|_| component.to_owned(), |(_, short)| short);
    json!({
        "method": "editor.set_property",
        "params": {
            "entity": entity.to_bits(),
            "component": component,
            "path": path,
            "value": value,
        }
    })
}

/// Draws the Inspector notice while it is current.
pub fn draw_notice(ui: &mut egui::Ui, world: &mut World) {
    let now = ui.input(|i| i.time);
    let Some(mut clip) = world.get_resource_mut::<InspectorClipboard>() else {
        return;
    };
    let Some(notice) = clip.notice.as_mut() else {
        return;
    };
    let shown_at = *notice.shown_at.get_or_insert(now);
    if now - shown_at > NOTICE_SECONDS {
        clip.notice = None;
        return;
    }
    let color = if notice.error {
        ui.visuals().warn_fg_color
    } else {
        ui.visuals().weak_text_color()
    };
    ui.colored_label(color, notice.text.clone());
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_secs_f64(NOTICE_SECONDS));
}

/// The component items of a header's context menu: Copy component, Paste component values, Reset
/// component. Returns whether one was picked (the caller closes the menu).
pub fn component_menu_items(ui: &mut egui::Ui, world: &mut World, entity: Entity, full: &str) -> bool {
    use crate::agent_parity::{
        INSPECTOR_COPY_COMPONENT, INSPECTOR_PASTE_COMPONENT, INSPECTOR_RESET_COMPONENT, action_button,
        action_enabled,
    };
    let can_paste = clip_matches(world, full);
    if action_button(ui, &INSPECTOR_COPY_COMPONENT).clicked() {
        let result = copy_property(world, entity, full, "");
        report(world, result, "Copied the component");
        return true;
    }
    if action_enabled(ui, can_paste, &INSPECTOR_PASTE_COMPONENT, None).clicked() {
        let result = paste_component(world, entity, false).map(drop);
        report(world, result, "Pasted the component's values");
        return true;
    }
    if action_button(ui, &INSPECTOR_RESET_COMPONENT).clicked() {
        let result = reset_property(world, entity, full, None);
        report(world, result, "Reset the component");
        return true;
    }
    false
}

/// The same component actions as small buttons at the top of a component's body (reachable with
/// Tab; the header menu needs a pointer).
pub fn component_tools_row(ui: &mut egui::Ui, world: &mut World, entity: Entity, full: &str, short: &str) {
    use crate::agent_parity::{
        INSPECTOR_COPY_COMPONENT, INSPECTOR_PASTE_COMPONENT, INSPECTOR_RESET_COMPONENT, action_button_text,
    };
    let can_paste = clip_matches(world, full);
    ui.horizontal(|ui| {
        let copy = action_button_text(
            ui,
            &INSPECTOR_COPY_COMPONENT,
            egui::RichText::new("\u{1f5d0}").small(),
        );
        if crate::a11y_audit::name(copy, egui::WidgetType::Button, &format!("Copy {short}")).clicked() {
            let result = copy_property(world, entity, full, "");
            report(world, result, "Copied the component");
        }
        let paste = ui.add_enabled_ui(can_paste, |ui| {
            action_button_text(
                ui,
                &INSPECTOR_PASTE_COMPONENT,
                egui::RichText::new("\u{1f4cb}").small(),
            )
        });
        let spoken = format!("Paste values into {short}");
        if crate::a11y_audit::name(paste.inner, egui::WidgetType::Button, &spoken).clicked() {
            let result = paste_component(world, entity, false).map(drop);
            report(world, result, "Pasted the component's values");
        }
        let reset = action_button_text(
            ui,
            &INSPECTOR_RESET_COMPONENT,
            egui::RichText::new("\u{21ba}").small(),
        );
        if crate::a11y_audit::name(
            reset,
            egui::WidgetType::Button,
            &format!("Reset {short} to default"),
        )
        .clicked()
        {
            let result = reset_property(world, entity, full, None);
            report(world, result, "Reset the component");
        }
    });
}

/// Paste as new component: a button under the component list while the clipboard holds a
/// component the entity does not have.
pub fn paste_new_button(ui: &mut egui::Ui, world: &mut World, entity: Entity) {
    let Some((type_path, is_component)) = world
        .get_resource::<InspectorClipboard>()
        .and_then(|clip| clip.entry.as_ref())
        .map(|entry| (entry.type_path.clone(), entry.is_component()))
    else {
        return;
    };
    if !is_component || crate::command::read_property(world, entity, &type_path, "").is_ok() {
        return;
    }
    let short = type_path.rsplit("::").next().unwrap_or(&type_path).to_owned();
    let response = crate::agent_parity::action_button_text(
        ui,
        &crate::agent_parity::INSPECTOR_PASTE_NEW_COMPONENT,
        format!("Paste {short} as new component"),
    );
    if response.clicked() {
        let result = paste_component(world, entity, true).map(drop);
        report(world, result, "Added the copied component");
    }
}

fn report(world: &mut World, result: Result<(), ClipError>, done: &str) {
    match result {
        Ok(()) => notify(world, done, false),
        Err(err) => notify(world, err.message, true),
    }
}

/// Whether the clipboard holds a whole component of type `full`.
fn clip_matches(world: &World, full: &str) -> bool {
    world
        .get_resource::<InspectorClipboard>()
        .and_then(|clip| clip.entry.as_ref())
        .is_some_and(|entry| entry.is_component() && entry.type_path == full)
}

// ---------------------------------------------------------------------------
// Agent methods
// ---------------------------------------------------------------------------

fn reason(err: ClipError) -> RpcError {
    let mut rpc = match err.code {
        "component_unknown" | "component_missing" | "path_not_found" => RpcError::not_found(
            err.message.clone(),
            "see editor.inspect_selection for components and paths",
        ),
        _ => RpcError::invalid_params(err.message.clone()),
    };
    match rpc.data.as_mut() {
        Some(Value::Object(data)) => {
            data.insert("reason".into(), Value::String(err.code.to_owned()));
        }
        _ => rpc.data = Some(json!({ "reason": err.code })),
    }
    rpc
}

fn str_param<'a>(params: &'a Value, name: &str, code: &'static str) -> Result<&'a str, RpcError> {
    params
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| reason(ClipError::new(code, format!("`{name}` must be a string"))))
}

fn index_param(params: &Value, name: &str) -> Result<usize, RpcError> {
    params
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| {
            reason(ClipError::new(
                "index_required",
                format!("`{name}` must be an index"),
            ))
        })
}

fn clipboard_json(world: &World) -> Value {
    world
        .get_resource::<InspectorClipboard>()
        .and_then(|clip| clip.entry.as_ref())
        .map_or(Value::Null, |entry| {
            json!({
                "type": entry.type_path,
                "ron": entry.ron,
                "component": entry.source.0,
                "path": entry.source.1,
            })
        })
}

/// `editor.property_copy`.
fn editor_property_copy(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::undo_agent::live_entity(world, params, "entity")?;
    let component = str_param(params, "component", "component_required")?.to_owned();
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    copy_property(world, entity, &component, &path).map_err(reason)?;
    Ok(json!({
        "entity": entity.to_bits(),
        "clipboard": clipboard_json(world),
        "agent_call": agent_call(world, entity, &component, &path),
    }))
}

/// `editor.property_paste`.
fn editor_property_paste(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::undo_agent::live_entity(world, params, "entity")?;
    regia_agent::require_not_feed_owned(world, entity)?;
    let component = str_param(params, "component", "component_required")?.to_owned();
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    paste_property(world, entity, &component, &path).map_err(reason)?;
    Ok(json!({
        "entity": entity.to_bits(),
        "component": component,
        "path": path,
        "applied": true,
        "undoable": true,
    }))
}

/// `editor.component_copy`.
fn editor_component_copy(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::undo_agent::live_entity(world, params, "entity")?;
    let component = str_param(params, "component", "component_required")?.to_owned();
    copy_property(world, entity, &component, "").map_err(reason)?;
    Ok(json!({ "entity": entity.to_bits(), "clipboard": clipboard_json(world) }))
}

/// `editor.component_paste`.
fn editor_component_paste(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::undo_agent::live_entity(world, params, "entity")?;
    regia_agent::require_not_feed_owned(world, entity)?;
    let as_new = match params.get("as_new") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(on)) => *on,
        Some(_) => return Err(RpcError::invalid_params("`as_new` must be a boolean")),
    };
    // `component`, when given, must name the clipboard's type (a guard against pasting the wrong
    // thing).
    if let Some(component) = params.get("component").and_then(Value::as_str) {
        let (full, _) = component_names(world, component).map_err(reason)?;
        let held = world
            .get_resource::<InspectorClipboard>()
            .and_then(|clip| clip.entry.as_ref())
            .map(|entry| entry.type_path.clone());
        if let Some(held) = held
            && held != full
        {
            return Err(reason(ClipError::new(
                "type_mismatch",
                format!("the clipboard holds {held}, not {full}"),
            )));
        }
    }
    let full = paste_component(world, entity, as_new).map_err(reason)?;
    Ok(json!({
        "entity": entity.to_bits(),
        "component": full,
        "as_new": as_new,
        "applied": true,
        "undoable": true,
    }))
}

/// `editor.reset_property`.
fn editor_reset_property(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::undo_agent::live_entity(world, params, "entity")?;
    regia_agent::require_not_feed_owned(world, entity)?;
    let component = str_param(params, "component", "component_required")?.to_owned();
    let all = params.get("all").and_then(Value::as_bool).unwrap_or(false);
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty());
    let path = match (all, path) {
        (true, None) => None,
        (false, Some(path)) => Some(path.to_owned()),
        (true, Some(_)) => {
            return Err(reason(ClipError::new(
                "path_and_all",
                "pass `path` or `all: true`, not both",
            )));
        }
        (false, None) => {
            return Err(reason(ClipError::new(
                "path_required",
                "pass `path` (a field) or `all: true` (the whole component)",
            )));
        }
    };
    reset_property(world, entity, &component, path.as_deref()).map_err(reason)?;
    Ok(json!({
        "entity": entity.to_bits(),
        "component": component,
        "path": path.unwrap_or_default(),
        "applied": true,
        "undoable": true,
    }))
}

/// `editor.list_move`.
fn editor_list_move(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::undo_agent::live_entity(world, params, "entity")?;
    regia_agent::require_not_feed_owned(world, entity)?;
    let component = str_param(params, "component", "component_required")?.to_owned();
    let path = str_param(params, "path", "path_required")?.to_owned();
    let from = index_param(params, "from")?;
    let to = index_param(params, "to")?;
    let len =
        list_edit(world, entity, &component, &path, ListOp::MoveElement { from, to }).map_err(reason)?;
    Ok(
        json!({ "entity": entity.to_bits(), "component": component, "path": path, "len": len, "undoable": true }),
    )
}

/// `editor.list_duplicate`.
fn editor_list_duplicate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::undo_agent::live_entity(world, params, "entity")?;
    regia_agent::require_not_feed_owned(world, entity)?;
    let component = str_param(params, "component", "component_required")?.to_owned();
    let path = str_param(params, "path", "path_required")?.to_owned();
    let index = index_param(params, "index")?;
    let len = list_edit(world, entity, &component, &path, ListOp::DuplicateElement(index)).map_err(reason)?;
    Ok(
        json!({ "entity": entity.to_bits(), "component": component, "path": path, "len": len, "undoable": true }),
    )
}

const ENTITY: ParamSchema = ParamSchema {
    name: "entity",
    kind: "number",
    required: true,
    description: "Entity id (from scene.query_entities).",
};

const COMPONENT: ParamSchema = ParamSchema {
    name: "component",
    kind: "string",
    required: true,
    description: "Component type name, short or full (\"PointLight\").",
};

/// The agent methods of the Inspector's field tools.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.property_copy",
                tool: "property_copy",
                description: "Copies a field (or the whole component with path \"\") to the Inspector \
                    clipboard, as the field menu's Copy value does. Answers the copied value's type \
                    and RON, and the editor.set_property call that writes it (Copy agent path).",
                params: &[
                    ENTITY,
                    COMPONENT,
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "Reflect path of the field; \"\" or absent = the whole component.",
                    },
                ],
                result: "{ entity, clipboard: { type, ron, component, path }, agent_call }",
                mutating: false,
                permission: Permission::Read,
            },
            editor_property_copy as Handler,
        ),
        (
            MethodSchema {
                name: "editor.property_paste",
                tool: "property_paste",
                description: "Pastes the Inspector clipboard into a field of the same type, one undo \
                    step (the field menu's Paste value). Refused with reason type_mismatch for \
                    another type, clipboard_empty when nothing was copied.",
                params: &[
                    ENTITY,
                    COMPONENT,
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "Reflect path of the field; \"\" or absent = the whole component.",
                    },
                ],
                result: "{ entity, component, path, applied, undoable }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_property_paste as Handler,
        ),
        (
            MethodSchema {
                name: "editor.component_copy",
                tool: "component_copy",
                description: "Copies a whole component to the Inspector clipboard (the header menu's \
                    Copy component).",
                params: &[ENTITY, COMPONENT],
                result: "{ entity, clipboard: { type, ron, component, path } }",
                mutating: false,
                permission: Permission::Read,
            },
            editor_component_copy as Handler,
        ),
        (
            MethodSchema {
                name: "editor.component_paste",
                tool: "component_paste",
                description: "Pastes a copied component, one undo step: its values onto the entity's \
                    component of the same type (Paste component values), or with as_new as a new \
                    component of an entity that lacks it (Paste as new component). Reasons: \
                    clipboard_empty, not_a_component, component_missing, component_present, \
                    type_mismatch.",
                params: &[
                    ENTITY,
                    ParamSchema {
                        name: "component",
                        kind: "string",
                        required: false,
                        description: "Expected component type; refused (type_mismatch) when the \
                            clipboard holds another.",
                    },
                    ParamSchema {
                        name: "as_new",
                        kind: "boolean",
                        required: false,
                        description: "true adds the component (the entity must not have it).",
                    },
                ],
                result: "{ entity, component, as_new, applied, undoable }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_component_paste as Handler,
        ),
        (
            MethodSchema {
                name: "editor.reset_property",
                tool: "reset_property",
                description: "Resets a field (path) or every field (all: true) to its default: the \
                    prefab's value on a prefab instance, else the type's Default. One undo step. \
                    Reasons: path_required, path_and_all, no_default, path_not_found.",
                params: &[
                    ENTITY,
                    COMPONENT,
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "Reflect path of the field.",
                    },
                    ParamSchema {
                        name: "all",
                        kind: "boolean",
                        required: false,
                        description: "true resets the whole component (Reset component).",
                    },
                ],
                result: "{ entity, component, path, applied, undoable }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_reset_property as Handler,
        ),
        (
            MethodSchema {
                name: "editor.list_move",
                tool: "list_move",
                description: "Moves a list element from one index to another (a drag of its handle, \
                    or Alt+Up/Down), one undo step. Reasons: not_a_list, index_out_of_range.",
                params: &[
                    ENTITY,
                    COMPONENT,
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Reflect path of the list (\"waypoints\").",
                    },
                    ParamSchema {
                        name: "from",
                        kind: "number",
                        required: true,
                        description: "Index of the element to move.",
                    },
                    ParamSchema {
                        name: "to",
                        kind: "number",
                        required: true,
                        description: "Index it ends at.",
                    },
                ],
                result: "{ entity, component, path, len, undoable }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_list_move as Handler,
        ),
        (
            MethodSchema {
                name: "editor.list_duplicate",
                tool: "list_duplicate",
                description: "Inserts a copy of a list element right after it (Duplicate item), one \
                    undo step. Reasons: not_a_list, index_out_of_range.",
                params: &[
                    ENTITY,
                    COMPONENT,
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Reflect path of the list.",
                    },
                    ParamSchema {
                        name: "index",
                        kind: "number",
                        required: true,
                        description: "Index of the element to copy.",
                    },
                ],
                result: "{ entity, component, path, len, undoable }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_list_duplicate as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modified_covers_the_field_and_its_children() {
        let tools = FieldTools {
            modified: ["offset.x".to_owned(), "tags[2]".to_owned(), "radius".to_owned()]
                .into_iter()
                .collect(),
            ..FieldTools::default()
        };
        assert!(tools.is_modified("radius"));
        assert!(tools.is_modified("offset"));
        assert!(tools.is_modified("offset.x"));
        assert!(tools.is_modified("tags"));
        assert!(!tools.is_modified("offs"), "a prefix of a name is not a parent");
        assert!(!tools.is_modified("rad"));
        assert!(!tools.is_modified("color"));
    }
}
