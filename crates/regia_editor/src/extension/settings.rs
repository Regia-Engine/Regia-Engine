// SPDX-License-Identifier: MPL-2.0
//! Typed settings extensions declare (ADR-7791 (2), spec addendum § 20.1).
//!
//! An extension registers a [`SettingDesc`] per setting: key `<ext>.<name>`, title, description,
//! [`SettingScope`], default (whose [`PrefValue`] kind is the setting's type), an optional range
//! for numbers and an optional list of choices for text. The settings show under the extension's
//! display name in Preferences (user scope) and Project Settings (project scope), and are read
//! with [`get`] (the stored value, else the default) and changed with [`set`] and [`reset`].
//!
//! Storage needs no new file format: a **user** value is kept in `editor_prefs.ron` under
//! `extension.<ext>.<name>` (the file already keeps keys outside its registry); a **project** value
//! is written to `Regia.toml` as `[extensions.settings.<ext>] <name> = value`, the table an
//! extension already reads at build time through `ExtensionContext::settings`.
//!
//! The agent surface is `editor.extension_settings` (`book/agents/editor-extensions.md`).

use std::collections::BTreeMap;

use regia_agent::protocol::{self, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_framework::extension::ExtensionRegistry;
use regia_framework::extension::registry::RegistrationKind;
use serde_json::{Value, json};

use super::context::EditorExtensions;
use crate::editor_prefs::{EditorPreferences, PrefValue};

/// Where a setting is stored and shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SettingScope {
    /// Per user, in Preferences (`editor_prefs.ron`).
    User,
    /// Per project, in Project Settings (`Regia.toml`).
    Project,
}

impl SettingScope {
    /// The lowercase name the agent surface uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

/// A typed setting an extension declares.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingDesc {
    /// `<ext>.<name>`; `name` is ASCII letters, digits and `_`.
    pub key: String,
    /// The label.
    pub title: String,
    /// One line shown as the hover text and listed by the agent.
    pub description: String,
    /// User or project.
    pub scope: SettingScope,
    /// The default; its kind (bool, whole number, number, text) is the setting's type.
    pub default: PrefValue,
    /// The least value of a number setting.
    pub min: Option<f64>,
    /// The greatest value of a number setting.
    pub max: Option<f64>,
    /// The allowed values of a text setting (empty: any text).
    pub choices: Vec<String>,
}

impl SettingDesc {
    /// The part of the key after `<ext>.`.
    #[must_use]
    pub fn name(&self) -> &str {
        self.key
            .split_once('.')
            .map_or(self.key.as_str(), |(_, name)| name)
    }

    /// The extension id (the part before the first `.`).
    #[must_use]
    pub fn extension(&self) -> &str {
        self.key.split_once('.').map_or("", |(ext, _)| ext)
    }

    /// `value` converted to the setting's kind (a whole number is taken for a number setting)
    /// when it fits the range and the choices.
    ///
    /// # Errors
    ///
    /// A message saying why it does not fit.
    pub fn check(&self, value: &PrefValue) -> Result<PrefValue, String> {
        let value = match (&self.default, value) {
            (PrefValue::Bool(_), PrefValue::Bool(b)) => PrefValue::Bool(*b),
            (PrefValue::Int(_), PrefValue::Int(i)) => PrefValue::Int(*i),
            (PrefValue::Float(_), PrefValue::Float(f)) => PrefValue::Float(*f),
            (PrefValue::Float(_), PrefValue::Int(i)) => PrefValue::Float(*i as f64),
            (PrefValue::Text(_), PrefValue::Text(t)) => PrefValue::Text(t.clone()),
            (default, _) => {
                return Err(format!("`{}` takes a {}", self.key, kind_name(default)));
            }
        };
        let number = match &value {
            PrefValue::Int(i) => Some(*i as f64),
            PrefValue::Float(f) => Some(*f),
            _ => None,
        };
        if let Some(number) = number {
            if !number.is_finite() {
                return Err(format!("`{}` takes a finite number", self.key));
            }
            if self.min.is_some_and(|min| number < min) || self.max.is_some_and(|max| number > max) {
                return Err(format!(
                    "`{}` takes {} to {}",
                    self.key,
                    self.min.map_or("any".to_owned(), |m| m.to_string()),
                    self.max.map_or("any".to_owned(), |m| m.to_string())
                ));
            }
        }
        if let PrefValue::Text(text) = &value
            && !self.choices.is_empty()
            && !self.choices.contains(text)
        {
            return Err(format!("`{}` takes one of {}", self.key, self.choices.join(", ")));
        }
        Ok(value)
    }
}

fn kind_name(value: &PrefValue) -> &'static str {
    match value {
        PrefValue::Bool(_) => "switch (true or false)",
        PrefValue::Int(_) => "whole number",
        PrefValue::Float(_) => "number",
        PrefValue::Text(_) => "text",
    }
}

/// Project values written this session, over what the extension registry read from `Regia.toml`
/// at startup (`None`: reset to the default).
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct ProjectSettingValues {
    /// Key → value written (or `None`, removed).
    pub values: BTreeMap<String, Option<PrefValue>>,
}

/// Why a setting was not changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingError {
    /// No live setting has the key.
    NotFound(String),
    /// The value does not fit (why).
    Invalid(String),
    /// A project setting with no project open.
    NoProject,
    /// `Regia.toml` could not be read or written (why).
    Write(String),
}

impl SettingError {
    /// The lowercase protocol code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "extension_setting_not_found",
            Self::Invalid(_) => "extension_setting_invalid",
            Self::NoProject => "no_project",
            Self::Write(_) => "extension_setting_write_failed",
        }
    }

    /// The message.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::NotFound(key) => format!("no extension setting `{key}`"),
            Self::Invalid(why) | Self::Write(why) => why.clone(),
            Self::NoProject => "no project is open in the editor".to_owned(),
        }
    }
}

/// Every live setting, `(owner, setting)` by key.
#[must_use]
pub fn settings(world: &World) -> Vec<(String, SettingDesc)> {
    let (Some(editor), Some(registry)) = (
        world.get_resource::<EditorExtensions>(),
        world.get_resource::<ExtensionRegistry>(),
    ) else {
        return Vec::new();
    };
    editor
        .settings
        .iter()
        .filter(|(key, (owner, _))| {
            registry.owner_of(RegistrationKind::EditorSetting, key) == Some(owner.as_str())
        })
        .map(|(_, (owner, desc))| (owner.clone(), desc.clone()))
        .collect()
}

fn find(world: &World, key: &str) -> Result<SettingDesc, SettingError> {
    settings(world)
        .into_iter()
        .map(|(_, desc)| desc)
        .find(|desc| desc.key == key)
        .ok_or_else(|| SettingError::NotFound(key.to_owned()))
}

fn prefs_key(key: &str) -> String {
    format!("extension.{key}")
}

/// A TOML value as a [`PrefValue`].
fn from_toml(value: &toml::Value) -> Option<PrefValue> {
    match value {
        toml::Value::Boolean(b) => Some(PrefValue::Bool(*b)),
        toml::Value::Integer(i) => Some(PrefValue::Int(*i)),
        toml::Value::Float(f) => Some(PrefValue::Float(*f)),
        toml::Value::String(s) => Some(PrefValue::Text(s.clone())),
        _ => None,
    }
}

fn to_toml(value: &PrefValue) -> toml::Value {
    match value {
        PrefValue::Bool(b) => toml::Value::Boolean(*b),
        PrefValue::Int(i) => toml::Value::Integer(*i),
        PrefValue::Float(f) => toml::Value::Float(*f),
        PrefValue::Text(t) => toml::Value::String(t.clone()),
    }
}

/// The stored value of `desc` (`None`: it holds its default). A stored value that no longer fits
/// (the extension changed the setting's type or range) reads as unset.
fn stored(world: &World, desc: &SettingDesc) -> Option<PrefValue> {
    let raw = match desc.scope {
        SettingScope::User => world
            .get_resource::<EditorPreferences>()
            .and_then(|prefs| prefs.extra(&prefs_key(&desc.key)).cloned()),
        SettingScope::Project => match world
            .get_resource::<ProjectSettingValues>()
            .and_then(|written| written.values.get(&desc.key))
        {
            Some(written) => written.clone(),
            None => world
                .get_resource::<ExtensionRegistry>()
                .and_then(|registry| registry.config().settings.get(desc.extension()).cloned())
                .and_then(|table| table.get(desc.name()).and_then(from_toml)),
        },
    };
    raw.and_then(|value| desc.check(&value).ok())
}

/// The effective value of the live setting `key` (stored, else the default). `None` when no live
/// setting has the key.
#[must_use]
pub fn get(world: &World, key: &str) -> Option<PrefValue> {
    let desc = find(world, key).ok()?;
    Some(stored(world, &desc).unwrap_or_else(|| desc.default.clone()))
}

/// Sets the live setting `key` (a value equal to the default is stored as unset). Returns the
/// effective value.
///
/// # Errors
///
/// See [`SettingError`].
pub fn set(world: &mut World, key: &str, value: &PrefValue) -> Result<PrefValue, SettingError> {
    let desc = find(world, key)?;
    let value = desc.check(value).map_err(SettingError::Invalid)?;
    let stored = (value != desc.default).then(|| value.clone());
    store(world, &desc, stored)?;
    Ok(value)
}

/// Returns the live setting `key` to its default. Returns the default.
///
/// # Errors
///
/// See [`SettingError`].
pub fn reset(world: &mut World, key: &str) -> Result<PrefValue, SettingError> {
    let desc = find(world, key)?;
    store(world, &desc, None)?;
    Ok(desc.default)
}

fn store(world: &mut World, desc: &SettingDesc, value: Option<PrefValue>) -> Result<(), SettingError> {
    match desc.scope {
        SettingScope::User => {
            world
                .get_resource_or_insert_with(EditorPreferences::default)
                .set_extra(&prefs_key(&desc.key), value);
            Ok(())
        }
        SettingScope::Project => {
            let root = world
                .get_resource::<crate::agent_ext::EditorProject>()
                .and_then(|project| project.root.clone())
                .ok_or(SettingError::NoProject)?;
            let path = root.join("Regia.toml");
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(error) => return Err(SettingError::Write(format!("{}: {error}", path.display()))),
            };
            let mut table: toml::Table = text.parse().map_err(|error: toml::de::Error| {
                SettingError::Write(format!("Regia.toml: {}", error.message()))
            })?;
            let extension = desc.extension().to_owned();
            let name = desc.name().to_owned();
            let cursor = nested_table(&mut table, &["extensions", "settings", extension.as_str()])
                .ok_or_else(|| {
                    SettingError::Write("Regia.toml: [extensions.settings] is not a table".to_owned())
                })?;
            match &value {
                Some(value) => {
                    cursor.insert(name, to_toml(value));
                }
                None => {
                    cursor.remove(&name);
                }
            }
            let text = toml::to_string(&table)
                .map_err(|error| SettingError::Write(format!("Regia.toml: {error}")))?;
            crate::level_io::write_atomic(&path, text.as_bytes()).map_err(SettingError::Write)?;
            world
                .get_resource_or_insert_with(ProjectSettingValues::default)
                .values
                .insert(desc.key.clone(), value);
            Ok(())
        }
    }
}

/// The settings of `scope` matching `query` (case-insensitive, over the extension name, title,
/// key and description), grouped under each extension's display name, each with a widget for its
/// type and a Reset button. Every control is a focusable widget. Draws nothing when no setting
/// matches.
pub fn settings_section(ui: &mut egui::Ui, world: &mut World, scope: SettingScope, query: &str) {
    let query = query.trim().to_lowercase();
    let mut groups: BTreeMap<String, Vec<SettingDesc>> = BTreeMap::new();
    for (owner, desc) in settings(world) {
        if desc.scope != scope {
            continue;
        }
        let group = super::actions::display_name(world, &owner);
        let haystack = format!("{group} {} {} {}", desc.title, desc.key, desc.description).to_lowercase();
        if query.is_empty() || haystack.contains(&query) {
            groups.entry(group).or_default().push(desc);
        }
    }
    if groups.is_empty() {
        return;
    }
    let mut changes: Vec<(String, Option<PrefValue>)> = Vec::new();
    ui.separator();
    for (group, rows) in &groups {
        ui.strong(group);
        egui::Grid::new(("extension_settings", scope.name(), group.as_str()))
            .num_columns(3)
            .show(ui, |ui| {
                for desc in rows {
                    ui.label(&desc.title).on_hover_text(&desc.description);
                    let current = stored(world, desc).unwrap_or_else(|| desc.default.clone());
                    if let Some(next) = value_widget(ui, desc, &current) {
                        changes.push((desc.key.clone(), Some(next)));
                    }
                    let is_default = current == desc.default;
                    if ui
                        .add_enabled(!is_default, egui::Button::new("Reset"))
                        .on_hover_text(format!("Reset {} to its default", desc.title))
                        .clicked()
                    {
                        changes.push((desc.key.clone(), None));
                    }
                    ui.end_row();
                }
            });
    }
    for (key, value) in changes {
        let result = match value {
            Some(value) => set(world, &key, &value).map(|_| ()),
            None => reset(world, &key).map(|_| ()),
        };
        if let Err(error) = result {
            tracing::warn!(target: "regia_editor", "{key}: {}", error.message());
        }
    }
}

/// The project-scope section of Project Settings: [`settings_section`] for project settings, and
/// each change mirrored into the panel's loaded `Regia.toml` table, so the panel's own Save keeps
/// it.
pub fn project_settings_section(
    ui: &mut egui::Ui,
    world: &mut World,
    panel: &mut crate::project_settings::ProjectSettingsState,
) {
    let before = world
        .get_resource::<ProjectSettingValues>()
        .cloned()
        .unwrap_or_default();
    settings_section(ui, world, SettingScope::Project, "");
    let Some(after) = world.get_resource::<ProjectSettingValues>().cloned() else {
        return;
    };
    if after == before {
        return;
    }
    let Some(table) = panel.manifest_mut() else {
        return;
    };
    for (key, value) in &after.values {
        if before.values.get(key) == Some(value) {
            continue;
        }
        let Some((extension, name)) = key.split_once('.') else {
            continue;
        };
        let Some(cursor) = nested_table(table, &["extensions", "settings", extension]) else {
            continue;
        };
        match value {
            Some(value) => {
                cursor.insert(name.to_owned(), to_toml(value));
            }
            None => {
                cursor.remove(name);
            }
        }
    }
}

/// The table at `path` under `table`, made on the way where missing; `None` when a step is not a
/// table.
fn nested_table<'a>(table: &'a mut toml::Table, path: &[&str]) -> Option<&'a mut toml::Table> {
    let Some((first, rest)) = path.split_first() else {
        return Some(table);
    };
    let next = table
        .entry((*first).to_owned())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()?;
    nested_table(next, rest)
}

/// The widget for one setting; the new value when the user changed it.
fn value_widget(ui: &mut egui::Ui, desc: &SettingDesc, current: &PrefValue) -> Option<PrefValue> {
    let label = desc.title.clone();
    match current.clone() {
        PrefValue::Bool(mut on) => crate::a11y_audit::bare_checkbox(ui, &mut on, &label)
            .on_hover_text(&label)
            .changed()
            .then_some(PrefValue::Bool(on)),
        PrefValue::Int(mut value) => {
            let mut drag = egui::DragValue::new(&mut value);
            if let (Some(min), Some(max)) = (desc.min, desc.max) {
                drag = drag.range(min as i64..=max as i64);
            }
            ui.add(drag)
                .on_hover_text(&label)
                .changed()
                .then_some(PrefValue::Int(value))
        }
        PrefValue::Float(mut value) => {
            let mut drag = egui::DragValue::new(&mut value).speed(0.05);
            if let (Some(min), Some(max)) = (desc.min, desc.max) {
                drag = drag.range(min..=max);
            }
            ui.add(drag)
                .on_hover_text(&label)
                .changed()
                .then_some(PrefValue::Float(value))
        }
        PrefValue::Text(mut text) if !desc.choices.is_empty() => {
            let before = text.clone();
            let _ = egui::ComboBox::from_id_salt(("extension_setting", desc.key.as_str()))
                .selected_text(&text)
                .show_ui(ui, |ui| {
                    for choice in &desc.choices {
                        ui.selectable_value(&mut text, choice.clone(), choice);
                    }
                })
                .response
                .on_hover_text(&label);
            (text != before).then_some(PrefValue::Text(text))
        }
        PrefValue::Text(mut text) => ui
            .add(egui::TextEdit::singleline(&mut text).desired_width(160.0))
            .on_hover_text(&label)
            .lost_focus()
            .then_some(PrefValue::Text(text)),
    }
}

// ---------------------------------------------------------------------------------------------
// Agent protocol

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

fn rpc(error: &SettingError) -> RpcError {
    let code = match error {
        SettingError::NotFound(_) | SettingError::NoProject => protocol::NOT_FOUND,
        SettingError::Invalid(_) => protocol::INVALID_PARAMS,
        SettingError::Write(_) => protocol::INTERNAL_ERROR,
    };
    RpcError::new(
        code,
        error.code(),
        error.message(),
        "editor.extension_settings with no key lists the settings, their types and ranges",
    )
}

fn row_json(world: &World, owner: &str, desc: &SettingDesc) -> Value {
    let value = stored(world, desc).unwrap_or_else(|| desc.default.clone());
    json!({
        "key": desc.key,
        "title": desc.title,
        "description": desc.description,
        "scope": desc.scope.name(),
        "type": match desc.default {
            PrefValue::Bool(_) => "bool",
            PrefValue::Int(_) => "int",
            PrefValue::Float(_) => "float",
            PrefValue::Text(_) => "text",
        },
        "value": value.to_json(),
        "default": desc.default.to_json(),
        "is_default": value == desc.default,
        "min": desc.min,
        "max": desc.max,
        "choices": desc.choices,
        "owner": owner,
        "extension": super::actions::display_name(world, owner),
    })
}

fn extension_settings(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(key) = params.get("key").and_then(Value::as_str) {
        let key = key.to_owned();
        let reset_it = params.get("reset").and_then(Value::as_bool).unwrap_or(false);
        if reset_it {
            reset(world, &key).map_err(|e| rpc(&e))?;
        } else if let Some(value) = params.get("value") {
            let value = PrefValue::from_json(value).ok_or_else(|| {
                rpc(&SettingError::Invalid(format!(
                    "`{key}`: the value is a switch, a number or text"
                )))
            })?;
            set(world, &key, &value).map_err(|e| rpc(&e))?;
        }
        let (owner, desc) = settings(world)
            .into_iter()
            .find(|(_, desc)| desc.key == key)
            .ok_or_else(|| rpc(&SettingError::NotFound(key.clone())))?;
        return Ok(row_json(world, &owner, &desc));
    }
    let rows: Vec<Value> = settings(world)
        .iter()
        .map(|(owner, desc)| row_json(world, owner, desc))
        .collect();
    Ok(json!({ "settings": rows }))
}

/// The agent method of extension settings, installed with the other extension methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.extension_settings",
            tool: "editor_extension_settings",
            description: "Lists the typed settings loaded extensions declare (key, title, description, user or project scope, type, value, default, range, choices, owning extension), or reads, sets or resets one by key, as the extension rows of Preferences and Project Settings do. User values are kept with the editor preferences; project values are written to Regia.toml's [extensions.settings.<extension>]. Errors: extension_setting_not_found, extension_setting_invalid, no_project, extension_setting_write_failed.",
            params: &[
                ParamSchema {
                    name: "key",
                    kind: "string",
                    required: false,
                    description: "The setting (<extension>.<name>); without it, every setting is listed.",
                },
                ParamSchema {
                    name: "value",
                    kind: "any",
                    required: false,
                    description: "The new value (a switch, a number or text, as the setting's type).",
                },
                ParamSchema {
                    name: "reset",
                    kind: "boolean",
                    required: false,
                    description: "Return the setting to its default.",
                },
            ],
            result: "{ settings: [{ key, title, description, scope, type, value, default, is_default, min, max, choices, owner, extension }] } or one such row",
            mutating: true,
            permission: Permission::EditScene,
        },
        extension_settings as Handler,
    )]
}
