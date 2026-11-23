// SPDX-License-Identifier: MPL-2.0
//! The standard Project Settings page (`docs/specs/19-editor.md` § 2, `docs/specs/21` § 2,
//! ADR-2321): the parts of `Regia.toml` the editor models beyond the general fields — the platform
//! targets (`[platforms.<os>]` tables), each target's quality preset (`render_profile`), the default
//! level (`[project] startup_level`, picked from the project's levels) and the organization — plus
//! the input maps in `config/input.ron` with their problems. The page and `editor.project_input_edit`
//! write that file immediately, in canonical RON (comments are not kept; the file is created when
//! it is missing).
//!
//! Edits go through [`ProjectSettingsState`] and keep every key the editor does not model. Save
//! refuses an unknown render profile, a default level outside `assets/` or not a level file, and a
//! target this editor does not know ([`problems`]).
//!
//! The agent surface is `editor.project_settings_get`, `editor.project_settings_set` and
//! `editor.project_input_edit` (`book/agents/editor-preferences.md`); a save by an agent reloads a
//! clean panel.

use std::path::{Path, PathBuf};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_render::ProfileName;
use serde_json::{Value, json};

use crate::project_settings::ProjectSettingsState;

/// The platform targets a project can ship to (`[platforms.<os>]`). macOS is testing-only
/// (ADR-009, ADR-264).
pub const PLATFORM_TARGETS: &[&str] = &["linux", "windows", "android", "macos"];

/// The quality presets a target can use, by `Regia.toml` name: the preset rows, then the
/// quality-ladder tiers with no row of their own (`VeryLow`, `Medium`; ADR-8844).
#[must_use]
pub fn render_profile_names() -> Vec<&'static str> {
    ProfileName::names()
}

/// The most level files the default-level picker lists.
const MAX_LEVELS: usize = 2000;

/// One thing wrong with the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsProblem {
    /// The `Regia.toml` key (`platforms.android.render_profile`).
    pub key: String,
    /// What is wrong.
    pub message: String,
    /// Whether Save refuses it (`false`: a warning).
    pub blocks_save: bool,
}

impl ProjectSettingsState {
    /// The platform targets the manifest has a `[platforms.<os>]` table for, in
    /// [`PLATFORM_TARGETS`] order, then any others.
    #[must_use]
    pub fn platform_targets(&self) -> Vec<String> {
        let Some(platforms) = self
            .manifest()
            .and_then(|t| t.get("platforms"))
            .and_then(toml::Value::as_table)
        else {
            return Vec::new();
        };
        let mut out: Vec<String> = PLATFORM_TARGETS
            .iter()
            .filter(|os| platforms.contains_key(**os))
            .map(|os| (*os).to_owned())
            .collect();
        out.extend(
            platforms
                .keys()
                .filter(|k| !PLATFORM_TARGETS.contains(&k.as_str()))
                .cloned(),
        );
        out
    }

    /// Adds (an empty `[platforms.<os>]` table) or removes a platform target. Removing drops the
    /// target's settings (Reload brings them back before a save).
    ///
    /// # Errors
    ///
    /// No manifest is loaded, or `os` is not in [`PLATFORM_TARGETS`].
    pub fn set_platform_target(&mut self, os: &str, enabled: bool) -> Result<bool, String> {
        if !PLATFORM_TARGETS.contains(&os) {
            return Err(format!(
                "unknown platform {os:?} (one of {})",
                PLATFORM_TARGETS.join(", ")
            ));
        }
        let platforms = self.platforms_table_mut()?;
        let changed = if enabled {
            if platforms.contains_key(os) {
                false
            } else {
                platforms.insert(os.to_owned(), toml::Value::Table(toml::Table::new()));
                true
            }
        } else {
            platforms.remove(os).is_some()
        };
        if changed {
            self.mark_dirty();
        }
        Ok(changed)
    }

    /// The `render_profile` (quality preset) of a target, if set.
    #[must_use]
    pub fn render_profile(&self, os: &str) -> Option<String> {
        self.manifest()
            .and_then(|t| t.get("platforms"))
            .and_then(|p| p.get(os))
            .and_then(|p| p.get("render_profile"))
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
    }

    /// Sets (canonical name) or clears a target's `render_profile`, adding the target when needed.
    ///
    /// # Errors
    ///
    /// No manifest, an unknown platform, or an unknown preset.
    pub fn set_render_profile(&mut self, os: &str, profile: Option<&str>) -> Result<(), String> {
        let name = match profile {
            Some(p) => Some(
                ProfileName::parse(p)
                    .ok_or_else(|| {
                        format!(
                            "unknown render profile {p:?} (one of {})",
                            render_profile_names().join(", ")
                        )
                    })?
                    .name(),
            ),
            None => None,
        };
        self.set_platform_target(os, true)?;
        let platforms = self.platforms_table_mut()?;
        let Some(table) = platforms.get_mut(os).and_then(toml::Value::as_table_mut) else {
            return Err(format!("[platforms.{os}] is not a table"));
        };
        match name {
            Some(name) => {
                table.insert("render_profile".to_owned(), toml::Value::String(name.to_owned()));
            }
            None => {
                table.remove("render_profile");
            }
        }
        self.mark_dirty();
        Ok(())
    }

    /// The `[platforms]` table, created when absent.
    fn platforms_table_mut(&mut self) -> Result<&mut toml::Table, String> {
        let root = self
            .manifest_mut()
            .ok_or_else(|| "no Regia.toml is loaded".to_owned())?;
        let platforms = root
            .entry("platforms".to_owned())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !platforms.is_table() {
            *platforms = toml::Value::Table(toml::Table::new());
        }
        platforms
            .as_table_mut()
            .ok_or_else(|| "[platforms] is not a table".to_owned())
    }
}

/// What is wrong with a manifest table (an empty list for a good one).
#[must_use]
pub fn problems(table: &toml::Table) -> Vec<SettingsProblem> {
    let mut out = Vec::new();
    let project = table.get("project").and_then(toml::Value::as_table);
    if let Some(level) = project
        .and_then(|p| p.get("startup_level"))
        .and_then(toml::Value::as_str)
        .filter(|l| !l.is_empty())
    {
        let path = Path::new(level);
        let escapes = path.is_absolute()
            || level.starts_with(['/', '\\'])
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir));
        if escapes {
            out.push(SettingsProblem {
                key: "project.startup_level".into(),
                message: format!("{level} is not a path under assets/"),
                blocks_save: true,
            });
        } else if !level.ends_with(".level.ron") && !level.ends_with(regia_scene::level_dir::LEVEL_DIR_SUFFIX)
        {
            out.push(SettingsProblem {
                key: "project.startup_level".into(),
                message: format!("{level} is not a level (.level.ron or a level folder)"),
                blocks_save: true,
            });
        }
    }
    if project
        .and_then(|p| p.get("version"))
        .and_then(toml::Value::as_str)
        .is_some_and(|v| v.trim().is_empty())
    {
        out.push(SettingsProblem {
            key: "project.version".into(),
            message: "the version is empty".into(),
            blocks_save: true,
        });
    }
    if let Some(platforms) = table.get("platforms").and_then(toml::Value::as_table) {
        for (os, value) in platforms {
            if !PLATFORM_TARGETS.contains(&os.as_str()) {
                out.push(SettingsProblem {
                    key: format!("platforms.{os}"),
                    message: format!("{os} is not a platform this editor knows; kept as is"),
                    blocks_save: false,
                });
                continue;
            }
            if let Some(profile) = value.get("render_profile") {
                let ok = profile.as_str().and_then(ProfileName::parse).is_some();
                if !ok {
                    out.push(SettingsProblem {
                        key: format!("platforms.{os}.render_profile"),
                        message: format!(
                            "{profile} is not a render profile (one of {})",
                            render_profile_names().join(", ")
                        ),
                        blocks_save: true,
                    });
                }
            }
        }
    }
    out
}

/// The project's levels, relative to `assets/` and sorted (`.level.ron` files and level folders).
#[must_use]
pub fn level_files(root: &Path) -> Vec<String> {
    let assets = root.join("assets");
    let mut out = Vec::new();
    let mut stack = vec![assets.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.to_string_lossy();
            let is_level =
                name.ends_with(".level.ron") || name.ends_with(regia_scene::level_dir::LEVEL_DIR_SUFFIX);
            if is_level {
                if let Ok(rel) = path.strip_prefix(&assets) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            } else if path.is_dir() && !name.ends_with(".regia") {
                stack.push(path);
            }
            if out.len() >= MAX_LEVELS {
                break;
            }
        }
    }
    out.sort();
    out
}

/// One input context of `config/input.ron`.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextSummary {
    /// Context name.
    pub name: String,
    /// Evaluation priority.
    pub priority: i32,
    /// Whether it hides its inputs from lower contexts.
    pub consume_lower: bool,
    /// `(action, input)` per binding, the input in RON.
    pub bindings: Vec<(String, String)>,
    /// Per binding (same order as `bindings`): its modifiers as a RON list, `[]` when none.
    pub modifiers: Vec<String>,
    /// Per binding (same order as `bindings`): its triggers as a RON list, `[]` when none.
    pub triggers: Vec<String>,
}

/// A snapshot of `config/input.ron`. The input section and `editor.project_input_edit` write that file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InputSummary {
    /// The file.
    pub path: PathBuf,
    /// Declared actions and their kinds.
    pub actions: Vec<(String, String)>,
    /// Contexts by name.
    pub contexts: Vec<ContextSummary>,
    /// Bindings that name an undeclared action, and actions nothing binds.
    pub problems: Vec<String>,
    /// Bindings that fight each other ([`input_conflicts`]).
    pub conflicts: Vec<String>,
}

/// Reads `config/input.ron`; `Ok(None)` when the project has none.
///
/// # Errors
///
/// The file cannot be read or parsed.
pub fn input_summary(root: &Path) -> Result<Option<InputSummary>, String> {
    let path = root.join("config/input.ron");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let config = regia_input::InputConfig::from_ron(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut summary = InputSummary {
        path,
        ..Default::default()
    };
    summary.actions = config
        .actions
        .iter()
        .map(|(name, kind)| (name.clone(), format!("{kind:?}")))
        .collect();
    let mut bound = std::collections::BTreeSet::new();
    for (name, context) in &config.contexts {
        let mut bindings = Vec::new();
        let (mut modifiers, mut triggers) = (Vec::new(), Vec::new());
        for binding in &context.bindings {
            let input = ron::to_string(&binding.input).unwrap_or_else(|_| format!("{:?}", binding.input));
            modifiers.push(ron::to_string(&binding.modifiers).unwrap_or_else(|_| "[]".to_owned()));
            triggers.push(ron::to_string(&binding.triggers).unwrap_or_else(|_| "[]".to_owned()));
            if !config.actions.contains_key(&binding.action) {
                summary.problems.push(format!(
                    "context {name}: a binding writes {:?}, which is not a declared action",
                    binding.action
                ));
            }
            bound.insert(binding.action.clone());
            bindings.push((binding.action.clone(), input));
        }
        summary.contexts.push(ContextSummary {
            name: name.clone(),
            priority: context.priority,
            consume_lower: context.consume_lower,
            bindings,
            modifiers,
            triggers,
        });
    }
    for action in config.actions.keys() {
        if !bound.contains(action) {
            summary.problems.push(format!("action {action} has no binding"));
        }
    }
    summary.conflicts = input_conflicts(&config);
    Ok(Some(summary))
}

/// The input as RON text (the key conflicts compare by).
fn input_text(binding: &regia_input::Binding) -> String {
    ron::to_string(&binding.input).unwrap_or_else(|_| format!("{:?}", binding.input))
}

/// The triggers a binding really has: none means [`regia_input::Trigger::Down`].
fn effective_triggers(binding: &regia_input::Binding) -> Vec<regia_input::Trigger> {
    if binding.triggers.is_empty() {
        vec![regia_input::Trigger::Down]
    } else {
        binding.triggers.clone()
    }
}

/// Bindings that fight each other, one line each, in a stable order:
///
/// - inside a context, two bindings on the same input that write different actions with a trigger
///   in common (both fire together);
/// - inside a context, the same input written to the same action twice (a duplicate);
/// - a context that consumes lower inputs and evaluates before another that binds the same input
///   (the lower binding never fires while the higher context is enabled). Contexts evaluate by
///   priority, highest first, and equal priorities by name (the runtime's order), so a consuming
///   context also hides an equal-priority context whose name sorts after its own.
#[must_use]
pub fn input_conflicts(config: &regia_input::InputConfig) -> Vec<String> {
    let mut out = Vec::new();
    for (name, context) in &config.contexts {
        for (i, a) in context.bindings.iter().enumerate() {
            for b in &context.bindings[i + 1..] {
                if a.input != b.input {
                    continue;
                }
                if a.action == b.action {
                    out.push(format!(
                        "context {name}: {} is bound to {} twice",
                        input_text(a),
                        a.action
                    ));
                } else if effective_triggers(a)
                    .iter()
                    .any(|t| effective_triggers(b).contains(t))
                {
                    out.push(format!(
                        "context {name}: {} is bound to {} and {} with the same trigger",
                        input_text(a),
                        a.action,
                        b.action
                    ));
                }
            }
        }
    }
    for (high_name, high) in &config.contexts {
        if !high.consume_lower {
            continue;
        }
        for (low_name, low) in &config.contexts {
            // The runtime's order (`regia_input` actions): priority descending, then name.
            let evaluates_first =
                high.priority > low.priority || (high.priority == low.priority && high_name < low_name);
            if !evaluates_first {
                continue;
            }
            for hb in &high.bindings {
                for lb in low.bindings.iter().filter(|lb| lb.input == hb.input) {
                    out.push(format!(
                        "context {high_name} (priority {}, consumes lower) hides {low_name}'s binding of {} to {}",
                        high.priority,
                        input_text(lb),
                        lb.action
                    ));
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Checks the parts of a config an edit can break: a chord names a declared action (not itself),
/// trigger times and modifier numbers are finite and positive where they must be.
fn validate_bindings(config: &regia_input::InputConfig) -> Result<(), String> {
    use regia_input::{Modifier, Trigger};
    for (name, context) in &config.contexts {
        for (index, binding) in context.bindings.iter().enumerate() {
            let at = format!("context {name}, binding {index} ({})", binding.action);
            for trigger in &binding.triggers {
                match trigger {
                    Trigger::Hold(t) | Trigger::Tap(t) | Trigger::Pulse(t)
                        if !(t.is_finite() && *t > 0.0) =>
                    {
                        return Err(format!(
                            "{at}: a trigger time must be a positive number of seconds"
                        ));
                    }
                    Trigger::Chord(other) if !config.actions.contains_key(other) => {
                        return Err(format!("{at}: Chord({other:?}) is not a declared action"));
                    }
                    Trigger::Chord(other) if *other == binding.action => {
                        return Err(format!("{at}: a binding cannot be chorded onto its own action"));
                    }
                    _ => {}
                }
            }
            for modifier in &binding.modifiers {
                let bad = match modifier {
                    Modifier::Smooth(rate) => !(rate.is_finite() && *rate >= 0.0),
                    Modifier::Scale(v) => !v.is_finite(),
                    Modifier::DeadZone(zone) => !(zone.lo.is_finite() && zone.hi.is_finite()),
                    _ => false,
                };
                if bad {
                    return Err(format!(
                        "{at}: a modifier has a number that is not finite (or negative smoothing)"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Draws the platform, default-level and input-map sections of the Project Settings panel.
pub fn extended_sections(ui: &mut egui::Ui, state: &mut ProjectSettingsState, root: &Path) {
    ui.horizontal(|ui| {
        ui.label("Organization:");
        let mut org = state.project_string("organization");
        if ui.text_edit_singleline(&mut org).changed() {
            state.set_project_string("organization", &org);
        }
    });
    let levels = level_files(root);
    let current = state.project_string("startup_level");
    ui.horizontal(|ui| {
        ui.label("Default level:");
        egui::ComboBox::from_id_salt("project_settings_default_level")
            .selected_text(if current.is_empty() {
                "(none)"
            } else {
                current.as_str()
            })
            .show_ui(ui, |ui| {
                if ui.selectable_label(current.is_empty(), "(none)").clicked() {
                    state.set_project_string("startup_level", "");
                }
                for level in &levels {
                    if ui.selectable_label(*level == current, level).clicked() {
                        state.set_project_string("startup_level", level);
                    }
                }
            });
    });

    ui.separator();
    ui.strong("Platforms and quality presets");
    let targets = state.platform_targets();
    let profiles = render_profile_names();
    egui::Grid::new("project_settings_platforms")
        .num_columns(3)
        .show(ui, |ui| {
            for os in PLATFORM_TARGETS {
                let mut on = targets.iter().any(|t| t == os);
                if ui.checkbox(&mut on, *os).changed()
                    && let Err(err) = state.set_platform_target(os, on)
                {
                    state.toast = err;
                }
                let profile = state.render_profile(os);
                ui.add_enabled_ui(on, |ui| {
                    egui::ComboBox::from_id_salt(("project_settings_profile", *os))
                        .selected_text(profile.as_deref().unwrap_or("(engine default)"))
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(profile.is_none(), "(engine default)")
                                .clicked()
                                && let Err(err) = state.set_render_profile(os, None)
                            {
                                state.toast = err;
                            }
                            for name in &profiles {
                                if ui
                                    .selectable_label(profile.as_deref() == Some(*name), *name)
                                    .clicked()
                                    && let Err(err) = state.set_render_profile(os, Some(*name))
                                {
                                    state.toast = err;
                                }
                            }
                        });
                });
                if *os == "macos" {
                    ui.weak("testing only");
                } else {
                    ui.label("");
                }
                ui.end_row();
            }
        });
    if let Some(table) = state.manifest() {
        for problem in problems(table) {
            let tokens = crate::theme::ui_tokens(ui.ctx());
            let color = if problem.blocks_save {
                tokens.error
            } else {
                tokens.warn
            };
            ui.colored_label(color, format!("{}: {}", problem.key, problem.message));
        }
    }

    ui.separator();
    egui::CollapsingHeader::new("Input maps (config/input.ron)")
        .id_salt("project_settings_input")
        .show(ui, |ui| input_section(ui, state, root));
    egui::CollapsingHeader::new("UI scale")
        .id_salt("project_settings_ui_scale")
        .show(ui, |ui| ui_scale_section(ui, root, &mut state.input_undo));
    egui::CollapsingHeader::new("Physics layers (config/physics.ron)")
        .id_salt("project_settings_physics_layers")
        .show(ui, |ui| {
            crate::physics_matrix::matrix_section(ui, root, &mut state.physics_undo)
        });
}

/// The text boxes of the input-map editor (kept in egui memory between frames).
#[derive(Clone, Debug, Default)]
struct InputDraft {
    /// Text of the modifier and trigger boxes by `context#index#m` / `context#index#t`.
    boxes: std::collections::BTreeMap<String, String>,
    /// The file's value each box was filled from: when the file changes under a box (undo, an
    /// agent edit, a removal that shifts the indices), the box is filled again.
    seeds: std::collections::BTreeMap<String, String>,
    action: String,
    kind: usize,
    context: String,
    bind_action: String,
    input: String,
}

impl InputDraft {
    /// The box `key`, filled from the file's value `now` the first time and again whenever the
    /// file's value differs from the one the box was filled from. A box kept across an outside
    /// change used to hold the old text, and "Set modifiers and triggers" wrote it back (an undone
    /// edit came back, or a removed binding's text landed on the binding that took its index).
    fn sync_box(&mut self, key: &str, now: &str) -> &mut String {
        let stale = self.seeds.get(key).is_none_or(|seed| seed != now);
        if stale {
            self.seeds.insert(key.to_owned(), now.to_owned());
            self.boxes.insert(key.to_owned(), now.to_owned());
        }
        self.boxes.entry(key.to_owned()).or_insert_with(|| now.to_owned())
    }
}

/// The input maps: actions and contexts with add and remove (written to `config/input.ron` at once).
fn input_section(ui: &mut egui::Ui, state: &mut ProjectSettingsState, root: &Path) {
    let summary = match input_summary(root) {
        Ok(summary) => summary,
        Err(err) => {
            ui.colored_label(crate::theme::ui_tokens(ui.ctx()).error, err);
            return;
        }
    };
    let id = egui::Id::new("project_settings_input_draft");
    let mut draft: InputDraft = ui.data_mut(|d| d.get_temp(id)).unwrap_or_default();
    let mut edits = Vec::new();
    match &summary {
        None => {
            ui.weak("No config/input.ron yet: adding an action creates it.");
        }
        Some(summary) => {
            ui.label(format!(
                "{} actions, {} contexts",
                summary.actions.len(),
                summary.contexts.len()
            ));
            for (name, kind) in &summary.actions {
                ui.horizontal(|ui| {
                    ui.label(format!("{name} ({kind})"));
                    if ui.small_button("Remove").clicked() {
                        edits.push(InputEdit::RemoveAction { name: name.clone() });
                    }
                });
            }
            for context in &summary.contexts {
                egui::CollapsingHeader::new(format!(
                    "{} (priority {}{})",
                    context.name,
                    context.priority,
                    if context.consume_lower {
                        ", consumes lower"
                    } else {
                        ""
                    }
                ))
                .id_salt(("project_settings_input_ctx", &context.name))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        context_priority_row(ui, context, &mut edits);
                    });
                    for (index, (action, input)) in context.bindings.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.weak(format!("{action} ← {input}"));
                            if ui.small_button("Remove").clicked() {
                                edits.push(InputEdit::RemoveBinding {
                                    context: context.name.clone(),
                                    index,
                                });
                            }
                        });
                        let current = [
                            context.modifiers.get(index).cloned().unwrap_or_default(),
                            context.triggers.get(index).cloned().unwrap_or_default(),
                        ];
                        ui.horizontal(|ui| {
                            let keys = [
                                format!("{}#{index}#m", context.name),
                                format!("{}#{index}#t", context.name),
                            ];
                            for (key, (hint, now)) in keys
                                .iter()
                                .zip([("modifiers", &current[0]), ("triggers", &current[1])])
                            {
                                let text = draft.sync_box(key, now);
                                ui.add(
                                    egui::TextEdit::singleline(text)
                                        .hint_text(hint)
                                        .desired_width(200.0)
                                        .font(egui::TextStyle::Monospace),
                                );
                            }
                            if ui.small_button("Set modifiers and triggers").clicked() {
                                if draft.boxes[&keys[0]] != current[0] {
                                    edits.push(InputEdit::SetModifiers {
                                        context: context.name.clone(),
                                        index,
                                        modifiers: draft.boxes[&keys[0]].clone(),
                                    });
                                }
                                if draft.boxes[&keys[1]] != current[1] {
                                    edits.push(InputEdit::SetTriggers {
                                        context: context.name.clone(),
                                        index,
                                        triggers: draft.boxes[&keys[1]].clone(),
                                    });
                                }
                            }
                        });
                    }
                });
            }
            for problem in &summary.problems {
                ui.colored_label(crate::theme::ui_tokens(ui.ctx()).warn, problem);
            }
            for conflict in &summary.conflicts {
                ui.colored_label(
                    crate::theme::ui_tokens(ui.ctx()).warn,
                    format!("conflict: {conflict}"),
                );
            }
        }
    }
    ui.horizontal(|ui| {
        ui.label("Add action:");
        ui.add(egui::TextEdit::singleline(&mut draft.action).desired_width(120.0));
        egui::ComboBox::from_id_salt("project_settings_input_kind")
            .selected_text(ACTION_KINDS[draft.kind.min(ACTION_KINDS.len() - 1)])
            .show_ui(ui, |ui| {
                for (i, kind) in ACTION_KINDS.iter().enumerate() {
                    ui.selectable_value(&mut draft.kind, i, *kind);
                }
            });
        if ui.button("Add").clicked() {
            edits.push(InputEdit::AddAction {
                name: draft.action.clone(),
                kind: ACTION_KINDS[draft.kind.min(ACTION_KINDS.len() - 1)].to_owned(),
            });
        }
    });
    ui.horizontal(|ui| {
        ui.label("Add binding:");
        ui.add(
            egui::TextEdit::singleline(&mut draft.context)
                .hint_text("Gameplay")
                .desired_width(90.0),
        );
        ui.add(
            egui::TextEdit::singleline(&mut draft.bind_action)
                .hint_text("action")
                .desired_width(90.0),
        );
        ui.add(
            egui::TextEdit::singleline(&mut draft.input)
                .hint_text("Key(\"Space\")")
                .desired_width(160.0),
        );
        if ui.button("Add").clicked() {
            let context = if draft.context.trim().is_empty() {
                regia_input::GAMEPLAY_CONTEXT.to_owned()
            } else {
                draft.context.trim().to_owned()
            };
            edits.push(InputEdit::AddBinding {
                context,
                action: draft.bind_action.trim().to_owned(),
                input: draft.input.clone(),
            });
        }
    });
    if !edits.is_empty() {
        state.toast = match edit_input_command(root, &edits) {
            Ok((_, command)) => {
                state.input_undo.push(command);
                // The boxes follow the file again after a write.
                draft.boxes.clear();
                draft.seeds.clear();
                "config/input.ron saved (Ctrl+Z undoes; comments in the file are not kept)".to_owned()
            }
            Err(err) => err.to_string(),
        };
    }
    ui.data_mut(|d| d.insert_temp(id, draft));
}

/// The priority row of one input context. While the pointer drags, the live value is kept as a
/// draft in egui memory (DragValue only reports it on dragged frames) and committed exactly
/// once — when the drag stops or the field loses focus, and only when the value changed — so
/// one drag is one `config/input.ron` write and one undo entry, not one per frame.
fn context_priority_row(ui: &mut egui::Ui, context: &ContextSummary, edits: &mut Vec<InputEdit>) {
    let draft_id = ui.id().with(("input_priority", &context.name));
    let mut priority = ui
        .data_mut(|d| d.get_temp::<i32>(draft_id))
        .unwrap_or(context.priority);
    let mut consume = context.consume_lower;
    ui.label("Priority");
    let response = ui.add(egui::DragValue::new(&mut priority));
    if response.dragged() || response.has_focus() {
        ui.data_mut(|d| d.insert_temp::<i32>(draft_id, priority));
    }
    let stopped = response.drag_stopped() || response.lost_focus();
    let toggled = ui.checkbox(&mut consume, "Consumes lower").changed();
    if stopped {
        ui.data_mut(|d| d.remove::<i32>(draft_id));
        if priority != context.priority {
            edits.push(InputEdit::SetContext {
                context: context.name.clone(),
                priority: Some(priority),
                consume_lower: None,
            });
        }
    }
    if toggled {
        edits.push(InputEdit::SetContext {
            context: context.name.clone(),
            priority: None,
            consume_lower: Some(consume),
        });
    }
}

/// The action kinds by their `config/input.ron` names.
pub const ACTION_KINDS: &[&str] = &["Button", "Axis1D", "Axis2D", "Axis3D"];

/// One edit of `config/input.ron`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEdit {
    /// Declare an action.
    AddAction {
        /// Its name.
        name: String,
        /// One of [`ACTION_KINDS`].
        kind: String,
    },
    /// Remove an action and every binding that writes it.
    RemoveAction {
        /// Its name.
        name: String,
    },
    /// Append a binding to a context (created when missing, priority 0).
    AddBinding {
        /// The context.
        context: String,
        /// A declared action.
        action: String,
        /// The input in RON, as in the file: `Key("Space")`, `GamepadButton(South)`, ...
        input: String,
    },
    /// Remove one binding of a context by its index.
    RemoveBinding {
        /// The context.
        context: String,
        /// The binding's position in the context.
        index: usize,
    },
    /// Set a context's priority and/or whether it consumes lower contexts' inputs (the context is
    /// created, priority 0, when missing).
    SetContext {
        /// The context.
        context: String,
        /// The new evaluation priority (higher first).
        priority: Option<i32>,
        /// The new `consume_lower`.
        consume_lower: Option<bool>,
    },
    /// Replace one binding's modifiers.
    SetModifiers {
        /// The context.
        context: String,
        /// The binding's position in the context.
        index: usize,
        /// A RON list of modifiers as in the file: `[Scale((2.0, 2.0, 1.0)), InvertY(true)]`;
        /// `[]` clears them.
        modifiers: String,
    },
    /// Replace one binding's triggers.
    SetTriggers {
        /// The context.
        context: String,
        /// The binding's position in the context.
        index: usize,
        /// A RON list of triggers as in the file: `[Pressed, Hold(0.5)]`; `[]` clears them.
        triggers: String,
    },
}

/// Why an input-map edit was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputEditError {
    /// The agent error code: `bad_input_edit`, `bad_input_source`, `input_config_read` or `input_config_write`.
    pub code: &'static str,
    /// What is wrong.
    pub message: String,
}

impl std::fmt::Display for InputEditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for InputEditError {}

fn input_error(code: &'static str, message: impl Into<String>) -> InputEditError {
    InputEditError {
        code,
        message: message.into(),
    }
}

/// Applies `edits` to `config/input.ron` in order, all or nothing, and writes the file atomically
/// (created when missing). Returns the new summary. The file is rewritten in canonical RON
/// (`InputConfig::try_to_ron`): comments and hand formatting are not kept (modifiers, triggers and
/// context priorities are kept and, since ADR-2771, editable).
///
/// # Errors
///
/// An edit names an unknown action, context or index, a duplicate or empty action, an unknown kind,
/// or an input that is not an input source; the existing file does not parse; the write failed.
pub fn edit_input(root: &Path, edits: &[InputEdit]) -> Result<InputSummary, InputEditError> {
    edit_input_command(root, edits).map(|(summary, _)| summary)
}

/// [`edit_input`] that also returns the undo entry for the write: push it on the editor's
/// [`UndoStack`](crate::command::UndoStack) so Ctrl+Z restores the file as it was.
///
/// What the save keeps and loses is what [`regia_input::InputConfig`] keeps: every action, context,
/// binding, modifier, trigger and the SpaceMouse section survive; comments, key order and hand
/// formatting do not (the file is rewritten in canonical RON, as it was before this editor
/// covered modifiers and triggers).
///
/// # Errors
///
/// As [`edit_input`].
pub fn edit_input_command(
    root: &Path,
    edits: &[InputEdit],
) -> Result<(InputSummary, InputConfigEdit), InputEditError> {
    let path = root.join("config/input.ron");
    let before_text = std::fs::read_to_string(&path).ok();
    let mut config = match &before_text {
        Some(text) => {
            regia_input::InputConfig::from_ron(text).map_err(|e| input_error("input_config_read", e))?
        }
        None if path.exists() => {
            return Err(input_error(
                "input_config_read",
                format!("cannot read {}", path.display()),
            ));
        }
        None => regia_input::InputConfig::default(),
    };
    for edit in edits {
        match edit {
            InputEdit::AddAction { name, kind } => {
                let name = name.trim();
                if name.is_empty() {
                    return Err(input_error("bad_input_edit", "an action needs a name"));
                }
                if config.actions.contains_key(name) {
                    return Err(input_error(
                        "bad_input_edit",
                        format!("action {name} already exists"),
                    ));
                }
                let kind = match kind.as_str() {
                    "Button" => regia_input::ActionKind::Button,
                    "Axis1D" => regia_input::ActionKind::Axis1D,
                    "Axis2D" => regia_input::ActionKind::Axis2D,
                    "Axis3D" => regia_input::ActionKind::Axis3D,
                    other => {
                        return Err(input_error(
                            "bad_input_edit",
                            format!("{other:?} is not an action kind ({})", ACTION_KINDS.join(", ")),
                        ));
                    }
                };
                config.actions.insert(name.to_owned(), kind);
            }
            InputEdit::RemoveAction { name } => {
                if config.actions.remove(name).is_none() {
                    return Err(input_error(
                        "bad_input_edit",
                        format!("no action is called {name}"),
                    ));
                }
                for context in config.contexts.values_mut() {
                    context.bindings.retain(|b| b.action != *name);
                }
            }
            InputEdit::AddBinding {
                context,
                action,
                input,
            } => {
                if !config.actions.contains_key(action) {
                    return Err(input_error(
                        "bad_input_edit",
                        format!("{action:?} is not a declared action"),
                    ));
                }
                let source: regia_input::InputSource = ron::from_str(input).map_err(|e| {
                    input_error(
                        "bad_input_source",
                        format!("{input:?} is not an input source: {e}"),
                    )
                })?;
                config
                    .contexts
                    .entry(context.clone())
                    .or_insert_with(|| regia_input::InputContext::new(0))
                    .bindings
                    .push(regia_input::Binding::new(action.clone(), source));
            }
            InputEdit::RemoveBinding { context, index } => {
                let ctx = config.contexts.get_mut(context).ok_or_else(|| {
                    input_error("bad_input_edit", format!("no context is called {context}"))
                })?;
                if *index >= ctx.bindings.len() {
                    return Err(input_error(
                        "bad_input_edit",
                        format!("context {context} has {} bindings", ctx.bindings.len()),
                    ));
                }
                ctx.bindings.remove(*index);
            }
            InputEdit::SetContext {
                context,
                priority,
                consume_lower,
            } => {
                if context.trim().is_empty() {
                    return Err(input_error("bad_input_edit", "a context needs a name"));
                }
                if priority.is_none() && consume_lower.is_none() {
                    return Err(input_error(
                        "bad_input_edit",
                        "set_context needs `priority` or `consume_lower`",
                    ));
                }
                let ctx = config
                    .contexts
                    .entry(context.trim().to_owned())
                    .or_insert_with(|| regia_input::InputContext::new(0));
                if let Some(priority) = priority {
                    ctx.priority = *priority;
                }
                if let Some(consume) = consume_lower {
                    ctx.consume_lower = *consume;
                }
            }
            InputEdit::SetModifiers {
                context,
                index,
                modifiers,
            } => {
                let list: Vec<regia_input::Modifier> = ron::from_str(modifiers).map_err(|e| {
                    input_error(
                        "bad_input_modifiers",
                        format!("{modifiers:?} is not a modifier list: {e}"),
                    )
                })?;
                binding_mut(&mut config, context, *index)?.modifiers = list;
            }
            InputEdit::SetTriggers {
                context,
                index,
                triggers,
            } => {
                let list: Vec<regia_input::Trigger> = ron::from_str(triggers).map_err(|e| {
                    input_error(
                        "bad_input_triggers",
                        format!("{triggers:?} is not a trigger list: {e}"),
                    )
                })?;
                binding_mut(&mut config, context, *index)?.triggers = list;
            }
        }
    }
    validate_bindings(&config).map_err(|e| input_error("bad_input_edit", e))?;
    let text = config
        .try_to_ron()
        .map_err(|e| input_error("input_config_write", e.to_string()))?;
    crate::level_io::write_atomic(&path, text.as_bytes())
        .map_err(|e| input_error("input_config_write", e))?;
    let summary = input_summary(root)
        .map_err(|e| input_error("input_config_read", e))?
        .ok_or_else(|| input_error("input_config_write", "the file vanished after the write"))?;
    let command = InputConfigEdit {
        path,
        before: before_text,
        after: Some(text),
        label: edits.len().to_string(),
    };
    Ok((summary, command))
}

/// The binding at `index` of `context`.
fn binding_mut<'a>(
    config: &'a mut regia_input::InputConfig,
    context: &str,
    index: usize,
) -> Result<&'a mut regia_input::Binding, InputEditError> {
    let ctx = config
        .contexts
        .get_mut(context)
        .ok_or_else(|| input_error("bad_input_edit", format!("no context is called {context}")))?;
    let count = ctx.bindings.len();
    ctx.bindings.get_mut(index).ok_or_else(|| {
        input_error(
            "bad_input_edit",
            format!("context {context} has {count} bindings"),
        )
    })
}

/// The undo entry of one input-map or UI-scale settings write: the file's text before and after. Applying it
/// writes `after`, reverting writes `before` (or removes the file when there was none).
#[derive(Debug, Clone)]
pub struct InputConfigEdit {
    path: PathBuf,
    before: Option<String>,
    after: Option<String>,
    label: String,
}

impl InputConfigEdit {
    fn write(&self, text: &Option<String>) {
        let result = match text {
            Some(text) => crate::level_io::write_atomic(&self.path, text.as_bytes()),
            None => std::fs::remove_file(&self.path).map_err(|e| e.to_string()),
        };
        if let Err(e) = result {
            tracing::warn!(target: "regia_editor", "{}: {e}", self.path.display());
        } else if self.path.ends_with("project.uisettings.ron") {
            UI_SCALE_REVISION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

impl crate::command::EditorCommand for InputConfigEdit {
    fn apply(&mut self, _world: &mut World) {
        self.write(&self.after.clone());
    }
    fn revert(&mut self, _world: &mut World) {
        self.write(&self.before.clone());
    }
    fn merge(&mut self, _next: &dyn crate::command::EditorCommand) -> bool {
        false
    }
    fn changes_level_state(&self) -> bool {
        // The level file is untouched: the input map has its own file, so this entry must not
        // mark a saved level dirty (which would prompt on close and trigger an autosave).
        false
    }
    fn touched(&self) -> Vec<crate::command::CommandTouch> {
        // The Undo History panel names the project file this entry rewrites.
        let document = if self.path.ends_with("project.uisettings.ron") {
            "assets/ui/project.uisettings.ron"
        } else {
            "config/input.ron"
        };
        vec![crate::command::CommandTouch::asset(document, None)]
    }
    fn description(&self) -> String {
        if self.path.ends_with("project.uisettings.ron") {
            "Edit UI scale".to_owned()
        } else {
            format!("Edit input maps ({} change(s))", self.label)
        }
    }
}

/// Pushes an input write's undo entry on the editor's undo stack, when there is one.
pub fn push_input_undo(world: &mut World, command: InputConfigEdit) {
    if let Some(mut stack) = world.get_resource_mut::<crate::command::UndoStack>() {
        stack.seal();
        stack.push(Box::new(command));
        stack.seal();
    }
}

/// Parses one `edits` entry of `editor.project_input_edit`.
fn input_edit_from_json(value: &Value) -> Result<InputEdit, RpcError> {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                bad(
                    "bad_input_edit",
                    format!("`{key}` must be a string"),
                    "see the op's fields",
                )
            })
    };
    match value.get("op").and_then(Value::as_str) {
        Some("add_action") => Ok(InputEdit::AddAction {
            name: text("name")?,
            kind: text("kind")?,
        }),
        Some("remove_action") => Ok(InputEdit::RemoveAction { name: text("name")? }),
        Some("add_binding") => Ok(InputEdit::AddBinding {
            context: value
                .get("context")
                .and_then(Value::as_str)
                .unwrap_or(regia_input::GAMEPLAY_CONTEXT)
                .to_owned(),
            action: text("action")?,
            input: text("input")?,
        }),
        Some("remove_binding") => Ok(InputEdit::RemoveBinding {
            context: text("context")?,
            index: value
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|i| usize::try_from(i).ok())
                .ok_or_else(|| bad("bad_input_edit", "`index` must be a whole number", ""))?,
        }),
        Some("set_context") => Ok(InputEdit::SetContext {
            context: text("context")?,
            priority: match value.get("priority") {
                None | Some(Value::Null) => None,
                Some(v) => Some(
                    v.as_i64()
                        .and_then(|p| i32::try_from(p).ok())
                        .ok_or_else(|| bad("bad_input_edit", "`priority` must be a whole number", ""))?,
                ),
            },
            consume_lower: match value.get("consume_lower") {
                None | Some(Value::Null) => None,
                Some(v) => Some(
                    v.as_bool()
                        .ok_or_else(|| bad("bad_input_edit", "`consume_lower` must be true or false", ""))?,
                ),
            },
        }),
        Some(op @ ("set_modifiers" | "set_triggers")) => {
            let index = value
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|i| usize::try_from(i).ok())
                .ok_or_else(|| bad("bad_input_edit", "`index` must be a whole number", ""))?;
            let context = text("context")?;
            if op == "set_modifiers" {
                Ok(InputEdit::SetModifiers {
                    context,
                    index,
                    modifiers: text("modifiers")?,
                })
            } else {
                Ok(InputEdit::SetTriggers {
                    context,
                    index,
                    triggers: text("triggers")?,
                })
            }
        }
        _ => Err(bad(
            "bad_input_edit",
            "`op` must be add_action, remove_action, add_binding, remove_binding, set_context, set_modifiers or set_triggers",
            "",
        )),
    }
}

/// `editor.project_input_edit { edits }`.
fn project_input_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = project_root(world)?;
    let edits = params
        .get("edits")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            bad(
                "bad_input_edit",
                "`edits` must be an array",
                "[{ op: \"add_action\", name, kind }]",
            )
        })?
        .iter()
        .map(input_edit_from_json)
        .collect::<Result<Vec<_>, _>>()?;
    if edits.is_empty() {
        // Nothing to apply: no rewrite (it would drop the file's comments, or create an empty
        // file), and no "0 change(s)" undo step.
        return Ok(settings_json_input(&root));
    }
    let (_, command) =
        edit_input_command(&root, &edits).map_err(|e| bad(e.code, e.message, "nothing was written"))?;
    push_input_undo(world, command);
    Ok(settings_json_input(&root))
}

/// The `input` part of [`settings_json`].
fn settings_json_input(root: &Path) -> Value {
    json!({ "input": input_json(root) })
}

/// Set by `editor.project_settings_set` after it saved: the shell reloads a clean panel.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProjectSettingsSaved;

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

fn bad(code: &str, message: impl Into<String>, hint: &str) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

fn project_root(world: &World) -> Result<PathBuf, RpcError> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or_else(|| {
            bad(
                "no_project",
                "no project is open",
                "open one with editor.open_project",
            )
        })
}

/// The JSON view of a loaded manifest.
#[must_use]
pub fn settings_json(state: &ProjectSettingsState, root: &Path) -> Value {
    let project = json!({
        "name": state.project_string("name"),
        "id": state.project_string("id"),
        "version": state.project_string("version"),
        "build_number": state.project_int("build_number"),
        "organization": state.project_string("organization"),
        "startup_level": state.project_string("startup_level"),
    });
    let platforms: serde_json::Map<String, Value> = PLATFORM_TARGETS
        .iter()
        .map(|os| {
            let enabled = state.platform_targets().iter().any(|t| t == os);
            (
                (*os).to_owned(),
                json!({ "enabled": enabled, "render_profile": state.render_profile(os) }),
            )
        })
        .collect();
    let problems: Vec<Value> = state
        .manifest()
        .map(problems)
        .unwrap_or_default()
        .into_iter()
        .map(|p| json!({ "key": p.key, "message": p.message, "blocks_save": p.blocks_save }))
        .collect();
    let input = input_json(root);
    json!({
        "project": project,
        "settings": state.manifest().map(settings_metadata).unwrap_or_default(),
        "platforms": platforms,
        "content": { "base_url": state.content_base_url() },
        "render_profiles": render_profile_names(),
        "levels": level_files(root),
        "input": input,
        "config_files": crate::project_settings::config_files(root),
        "problems": problems,
        // The Physics (the layer matrix view, ADR-7267), Localization, Packaging and Audio pages
        // (ADR-7234).
        "pages": crate::creators::pages_json(state, root),
    })
}

/// The JSON view of the input maps (`null` without `config/input.ron`).
fn input_json(root: &Path) -> Value {
    match input_summary(root) {
        Ok(None) => Value::Null,
        Err(err) => json!({ "error": err }),
        Ok(Some(s)) => json!({
            "path": s.path.display().to_string(),
            "actions": s.actions.iter().map(|(n, k)| json!({ "name": n, "kind": k })).collect::<Vec<_>>(),
            "contexts": s.contexts.iter().map(|c| json!({
                "name": c.name,
                "priority": c.priority,
                "consume_lower": c.consume_lower,
                "bindings": c.bindings.iter().enumerate().map(|(n, (a, i))| json!({
                    "action": a,
                    "input": i,
                    "modifiers": c.modifiers.get(n),
                    "triggers": c.triggers.get(n),
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "problems": s.problems,
            "conflicts": s.conflicts,
        }),
    }
}

/// `editor.project_settings_get`.
fn project_settings_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let root = project_root(world)?;
    let mut state = ProjectSettingsState::default();
    state
        .load(&root)
        .map_err(|e| bad("project_settings_read", e, "check Regia.toml parses"))?;
    Ok(settings_json(&state, &root))
}

/// `editor.project_settings_set { project?, platforms?, save? }`.
fn project_settings_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = project_root(world)?;
    let mut state = ProjectSettingsState::default();
    state
        .load(&root)
        .map_err(|e| bad("project_settings_read", e, "check Regia.toml parses"))?;
    reset_settings(&mut state, params)?;
    if let Some(project) = params.get("project").filter(|v| !v.is_null()) {
        let object = project.as_object().ok_or_else(|| {
            bad(
                "bad_settings",
                "`project` must be an object",
                "{ name, version, build_number, organization, startup_level, id }",
            )
        })?;
        for (key, value) in object {
            match (key.as_str(), value) {
                ("name" | "id" | "version" | "organization" | "startup_level", Value::String(s)) => {
                    state.set_project_string(key, s);
                }
                ("build_number", v) if v.as_i64().is_some_and(|n| (0..=i64::from(u32::MAX)).contains(&n)) => {
                    state.set_project_int(key, v.as_i64().unwrap_or(1));
                }
                _ => {
                    return Err(bad(
                        "bad_settings",
                        format!("`project.{key}` is not a field this method sets, or has the wrong type"),
                        "strings: name, id, version, organization, startup_level; integer: build_number",
                    ));
                }
            }
        }
    }
    if let Some(platforms) = params.get("platforms").filter(|v| !v.is_null()) {
        let object = platforms.as_object().ok_or_else(|| {
            bad(
                "bad_settings",
                "`platforms` must be an object",
                "{ android: { enabled: true, render_profile: \"MobileHigh\" } }",
            )
        })?;
        for (os, entry) in object {
            if !PLATFORM_TARGETS.contains(&os.as_str()) {
                return Err(bad(
                    "unknown_platform",
                    format!("{os} is not a platform target"),
                    "linux, windows, android, macos",
                ));
            }
            if let Some(enabled) = entry.get("enabled") {
                let enabled = enabled
                    .as_bool()
                    .ok_or_else(|| bad("bad_settings", "`enabled` must be a boolean", "true or false"))?;
                state
                    .set_platform_target(os, enabled)
                    .map_err(|e| bad("bad_settings", e, ""))?;
            }
            if let Some(profile) = entry.get("render_profile") {
                let profile = match profile {
                    Value::Null => None,
                    Value::String(s) => Some(s.as_str()),
                    _ => {
                        return Err(bad(
                            "bad_render_profile",
                            "`render_profile` must be a string or null",
                            "",
                        ));
                    }
                };
                state.set_render_profile(os, profile).map_err(|e| {
                    bad(
                        "bad_render_profile",
                        e,
                        "see render_profiles in editor.project_settings_get",
                    )
                })?;
            }
        }
    }
    // The [content] section's chunk host field (ADR-7747); "" removes the key, as in the panel.
    if let Some(content) = params.get("content").filter(|v| !v.is_null()) {
        let object = content.as_object().ok_or_else(|| {
            bad(
                "bad_settings",
                "`content` must be an object",
                "{ base_url: \"https://cdn.example.com/game\" }",
            )
        })?;
        for (key, value) in object {
            match (key.as_str(), value) {
                ("base_url", Value::String(url)) => state.set_content_base_url(url.trim()),
                _ => {
                    return Err(bad(
                        "bad_settings",
                        format!("`content.{key}` is not a field this method sets, or has the wrong type"),
                        "string: base_url (\"\" removes it)",
                    ));
                }
            }
        }
    }
    if let Some(hygiene) = params.get("package_hygiene").filter(|value| !value.is_null()) {
        let mut limits = crate::creators::HygieneLimits::read(&state)
            .map_err(|error| error.rpc("see book/agents/editor-preferences.md"))?;
        limits
            .apply(hygiene)
            .map_err(|error| error.rpc("see book/agents/editor-preferences.md"))?;
        limits
            .write(&mut state)
            .map_err(|error| error.rpc("see book/agents/editor-preferences.md"))?;
    }
    let blocking: Vec<String> = state
        .manifest()
        .map(problems)
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.blocks_save)
        .map(|p| format!("{}: {}", p.key, p.message))
        .collect();
    if !blocking.is_empty() {
        return Err(bad(
            "project_settings_invalid",
            blocking.join("; "),
            "nothing was written; fix the values and call again",
        ));
    }
    let save = params.get("save").and_then(Value::as_bool).unwrap_or(true);
    if save && state.is_dirty() {
        let path = regia_core::brand::project_manifest(&root)
            .unwrap_or_else(|| root.join(regia_core::brand::MANIFEST_FILE));
        let before = std::fs::read_to_string(&path)
            .map_err(|e| bad("project_settings_read", e.to_string(), "nothing was written"))?;
        state
            .save(&root)
            .map_err(|e| bad("project_settings_invalid", e, "nothing was written"))?;
        let after = std::fs::read_to_string(&path)
            .map_err(|e| bad("project_settings_read", e.to_string(), "manifest was saved"))?;
        if before != after {
            let mut stack = world.get_resource_or_insert_with(crate::command::UndoStack::default);
            stack.seal();
            stack.push(Box::new(ManifestEdit(InputConfigEdit {
                path,
                before: Some(before),
                after: Some(after),
                label: String::new(),
            })));
            stack.seal();
        }
        invalidate_settings(world);
    }
    let mut out = settings_json(&state, &root);
    out["saved"] = json!(save);
    Ok(out)
}

/// The Project Settings agent methods.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.project_settings_get",
                tool: "project_settings_get",
                description: "The open project's settings as the Project Settings panel shows them: name, id, version, build number, organization, default level (with the project's levels), platform targets with their quality presets (render_profile), the input maps of config/input.ron (this call does not write them; editor.project_input_edit rewrites the file at once) and the manifest's problems.",
                params: &[],
                result: "{ settings: [{ key, value, default, changed, resettable }], project: { name, id, version, build_number, organization, startup_level }, platforms: { linux|windows|android|macos: { enabled, render_profile } }, render_profiles, levels, input: { path, actions: [{ name, kind }], contexts: [{ name, priority, consume_lower, bindings: [{ action, input, modifiers, triggers }] }], problems, conflicts } | { error } | null, config_files, problems: [{ key, message, blocks_save }], pages: { physics: { path, layers, collides, queries }, localization: { path, source, locales }, packaging: { android, linux }, package_hygiene: { max_memory_bytes, max_file_bytes, max_total_bytes, max_work, scan_window_bytes, max_matcher_bytes }, audio: { path, buses } } }",
                mutating: false,
                permission: Permission::Read,
            },
            project_settings_get as Handler,
        ),
        (
            MethodSchema {
                name: "editor.project_settings_set",
                tool: "project_settings_set",
                description: "Edit the open project's Regia.toml as the Project Settings panel does, keeping every key it does not model: project fields, platform targets and their quality presets, the [content] chunk host, and [package_hygiene] memory, file, total, work, scan-window and matcher ceilings. Validated as the panel's Save validates (nothing is written when a value is refused); saves by default in one undo step. reset_keys or reset_category restores documented defaults.",
                params: &[
                    ParamSchema {
                        name: "reset_keys",
                        kind: "array",
                        required: false,
                        description: "Dotted keys to revert to documented defaults in one undo step. Unknown defaults are refused. Exclusive with reset_category.",
                    },
                    ParamSchema {
                        name: "reset_category",
                        kind: "string",
                        required: false,
                        description: "Reset every documented key in a dotted category (project, content, platforms.android, etc.) in one undo step, regardless of the search filter.",
                    },
                    ParamSchema {
                        name: "project",
                        kind: "object",
                        required: false,
                        description: "{ name?, id?, version?, build_number?, organization?, startup_level? } (startup_level relative to assets/, \"\" clears it).",
                    },
                    ParamSchema {
                        name: "platforms",
                        kind: "object",
                        required: false,
                        description: "{ <os>: { enabled?, render_profile? } } for linux, windows, android, macos; render_profile null returns to the engine default.",
                    },
                    ParamSchema {
                        name: "content",
                        kind: "object",
                        required: false,
                        description: "{ base_url? }: the [content] chunk host (\"\" removes the key).",
                    },
                    ParamSchema {
                        name: "package_hygiene",
                        kind: "object",
                        required: false,
                        description: "{ max_memory_bytes?, max_file_bytes?, max_total_bytes?, max_work?, scan_window_bytes?, max_matcher_bytes? }: positive integers written into [package_hygiene]; other keys in that table are kept. scan_window_bytes must not exceed max_memory_bytes.",
                    },
                    ParamSchema {
                        name: "save",
                        kind: "boolean",
                        required: false,
                        description: "Write Regia.toml (default true); false only validates and returns the result.",
                    },
                ],
                result: "the editor.project_settings_get result plus saved",
                mutating: true,
                permission: Permission::EditScene,
            },
            project_settings_set as Handler,
        ),
        (
            MethodSchema {
                name: "editor.project_input_edit",
                tool: "project_input_edit",
                description: "Edit the open project's input maps (config/input.ron) as the Project Settings page does: add or remove actions and bindings, set a context's priority and consume_lower, and set a binding's modifiers and triggers, in order, all or nothing; the file is created when missing and rewritten in canonical RON (comments are lost, every modifier, trigger and the spacemouse section is kept). Undoable with editor.undo.",
                params: &[ParamSchema {
                    name: "edits",
                    kind: "array",
                    required: true,
                    description: "[{ op: \"add_action\", name, kind: Button|Axis1D|Axis2D|Axis3D } | { op: \"remove_action\", name } | { op: \"add_binding\", context?, action, input: \"Key(\\\"Space\\\")\" } | { op: \"remove_binding\", context, index } | { op: \"set_context\", context, priority?, consume_lower? } | { op: \"set_modifiers\", context, index, modifiers: \"[Scale((2.0, 2.0, 1.0))]\" } | { op: \"set_triggers\", context, index, triggers: \"[Pressed, Hold(0.5)]\" }]",
                }],
                result: "{ input: { path, actions, contexts: [{ name, priority, consume_lower, bindings: [{ action, input, modifiers, triggers }] }], problems, conflicts } }",
                mutating: true,
                permission: Permission::EditScene,
            },
            project_input_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ui_scale_settings",
                tool: "ui_scale_settings",
                description: "Read or edit assets/ui/project.uisettings.ron (v2, ADR-8252), preserving the group layer budget. Set validates then writes atomically and adds one editor undo entry. Send dpi_curve and/or scale_mode; an omitted field stays and null clears it. scale_mode is the saved project rule, applied to roots still on the engine default. A bad curve or rule uses the error code ui_scale_curve. Preview rule overrides belong to editor.ui_designer_preview.",
                params: &[
                    ParamSchema {
                        name: "action",
                        kind: "string",
                        required: false,
                        description: "get (default) or set.",
                    },
                    ParamSchema {
                        name: "dpi_curve",
                        kind: "array",
                        required: false,
                        description: "set: increasing [display DPI, scale] points, both positive and finite; null removes the curve. Applies to ConstantPhysicalSize roots. Omit to leave the saved curve.",
                    },
                    ParamSchema {
                        name: "scale_mode",
                        kind: "any",
                        required: false,
                        description: "set: a ScaleMode JSON value (\"ConstantPixelSize\", \"ConstantPhysicalSize\", {\"ScaleWithScreen\":{\"reference\":[1920,1080],\"match_width\":0.5}}, {\"IntegerScale\":{\"reference\":[320,180]}}); null clears the saved rule. Omit to leave it. References must be finite and above zero; match_width is 0 to 1.",
                    },
                ],
                result: "{ path, version, dpi_curve, scale_mode, layer_budget_bytes, applied }",
                mutating: true,
                permission: Permission::EditScene,
            },
            ui_scale_settings as Handler,
        ),
    ]
}

// ---------------------------------------------------------------------------------------------
// The Settings window's shared search (ADR-7767)

// ADR-8196: None is unknown; Some(None) is a known default represented by removing a key.
fn manifest_default(key: &str) -> Option<Option<toml::Value>> {
    match key {
        "project.version" => Some(Some(toml::Value::String("0.1.0".into()))),
        "project.build_number" => Some(Some(toml::Value::Integer(1))),
        "project.organization" | "project.startup_level" => Some(Some(toml::Value::String(String::new()))),
        "content.base_url" => Some(None),
        _ if PLATFORM_TARGETS
            .iter()
            .any(|os| key == format!("platforms.{os}.render_profile")) =>
        {
            Some(None)
        }
        _ => None,
    }
}

fn row_metadata(key: &str, value: &toml::Value) -> (Option<String>, Option<bool>) {
    match manifest_default(key) {
        Some(Some(default)) => (Some(default.to_string()), Some(value != &default)),
        Some(None) => (Some("(unset)".into()), Some(true)),
        None => (None, None),
    }
}

/// Metadata for every present manifest key. Unknown defaults are null and never resettable.
#[must_use]
pub fn settings_metadata(table: &toml::Table) -> Vec<Value> {
    manifest_rows(table)
        .into_iter()
        .map(|row| {
            json!({ "key": row.key, "value": row.value, "default": row.default,
            "changed": row.changed, "resettable": row.changed.is_some() })
        })
        .collect()
}

fn reset_settings(state: &mut ProjectSettingsState, params: &Value) -> Result<(), RpcError> {
    let keys = params.get("reset_keys").filter(|v| !v.is_null());
    let category = params.get("reset_category").filter(|v| !v.is_null());
    if keys.is_none() && category.is_none() {
        return Ok(());
    }
    if keys.is_some() && category.is_some() {
        return Err(bad(
            "bad_settings",
            "choose reset_keys or reset_category",
            "nothing was written",
        ));
    }
    let table = state
        .manifest()
        .ok_or_else(|| bad("bad_settings", "no manifest", "open a project"))?;
    let selected: Vec<String> = if let Some(keys) = keys {
        let keys = keys.as_array().ok_or_else(|| {
            bad(
                "bad_settings",
                "reset_keys must be an array",
                "array of dotted keys",
            )
        })?;
        keys.iter()
            .map(|key| {
                let key = key
                    .as_str()
                    .ok_or_else(|| bad("bad_settings", "reset key must be a string", "dotted key"))?;
                if manifest_default(key).is_none() {
                    return Err(bad(
                        "bad_settings",
                        format!("no documented default for {key}"),
                        "check resettable in editor.project_settings_get",
                    ));
                }
                Ok(key.to_owned())
            })
            .collect::<Result<_, _>>()?
    } else {
        let category = category
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .ok_or_else(|| {
                bad(
                    "bad_settings",
                    "reset_category must be a nonempty string",
                    "for example project or platforms.android",
                )
            })?;
        manifest_rows(table)
            .into_iter()
            .filter(|row| {
                row.changed.is_some()
                    && row
                        .key
                        .rsplit_once('.')
                        .is_some_and(|(parent, _)| parent == category)
            })
            .map(|row| row.key)
            .collect()
    };
    let mut changed = false;
    if let Some(table) = state.manifest_mut() {
        for key in selected {
            let mut parts = key.split('.').peekable();
            let mut parent = &mut *table;
            while let Some(part) = parts.next() {
                if parts.peek().is_none() {
                    match manifest_default(&key) {
                        Some(Some(value)) if parent.get(part) != Some(&value) => {
                            parent.insert(part.to_owned(), value);
                            changed = true;
                        }
                        Some(None) => changed |= parent.remove(part).is_some(),
                        _ => {}
                    }
                    break;
                }
                let Some(next) = parent.get_mut(part).and_then(toml::Value::as_table_mut) else {
                    break;
                };
                parent = next;
            }
        }
    }
    if changed {
        state.mark_dirty();
    }
    Ok(())
}

#[derive(Debug)]
struct ManifestEdit(InputConfigEdit);

fn invalidate_settings(world: &mut World) {
    world.insert_resource(ProjectSettingsSaved);
    if let Some(mut panel) = world.get_resource_mut::<crate::prefs_panel::PrefsPanelState>() {
        panel.project_loaded_for = None;
    }
}

impl crate::command::EditorCommand for ManifestEdit {
    fn apply(&mut self, world: &mut World) {
        self.0.write(&self.0.after);
        invalidate_settings(world);
    }
    fn revert(&mut self, world: &mut World) {
        self.0.write(&self.0.before);
        invalidate_settings(world);
    }
    fn merge(&mut self, _next: &dyn crate::command::EditorCommand) -> bool {
        false
    }
    fn changes_level_state(&self) -> bool {
        false
    }
    fn description(&self) -> String {
        "Edit project settings".into()
    }
}

/// Resets documented defaults through the same operation as `editor.project_settings_set`.
///
/// # Errors
/// The project cannot be read/written, or a requested key has no documented default.
pub fn revert_project_settings(
    world: &mut World,
    keys: &[String],
    category: Option<&str>,
) -> Result<Value, RpcError> {
    let params = if let Some(category) = category {
        json!({"reset_category": category})
    } else {
        json!({"reset_keys": keys})
    };
    project_settings_set(world, &params)
}

/// One `Regia.toml` key the Settings window's search lists: the dotted key and its value as TOML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSettingRow {
    /// The dotted key (`platforms.android.render_profile`).
    pub key: String,
    /// The value as TOML text (an array or inline table on one line).
    pub value: String,
    /// Documented default as TOML, `(unset)` for an absent override, or None if unknown.
    pub default: Option<String>,
    /// Whether this row differs from its default; None if no default is defined.
    pub changed: Option<bool>,
}

/// Every leaf key of the manifest (tables flattened to dotted keys), sorted.
#[must_use]
pub fn manifest_rows(table: &toml::Table) -> Vec<ProjectSettingRow> {
    fn walk(prefix: &str, table: &toml::Table, out: &mut Vec<ProjectSettingRow>) {
        for (key, value) in table {
            let key = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            match value {
                toml::Value::Table(inner) => walk(&key, inner, out),
                other => {
                    let (default, changed) = row_metadata(&key, other);
                    out.push(ProjectSettingRow {
                        key,
                        value: other.to_string(),
                        default,
                        changed,
                    });
                }
            }
        }
    }
    let mut out = Vec::new();
    walk("", table, &mut out);
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out
}

/// The rows whose key or value contains every word of `query` (case-insensitive); an empty query
/// lists every row.
#[must_use]
pub fn search_rows<'a>(rows: &'a [ProjectSettingRow], query: &str) -> Vec<&'a ProjectSettingRow> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    rows.iter()
        .filter(|row| {
            let hay = format!("{} {}", row.key, row.value).to_lowercase();
            words.iter().all(|w| hay.contains(w))
        })
        .collect()
}

/// The `Regia.toml` rows of the project at `root` matching `query` (read from disk; `Err` when the
/// manifest cannot be read).
///
/// # Errors
///
/// The manifest cannot be read or parsed.
pub fn search_project_settings(root: &Path, query: &str) -> Result<Vec<ProjectSettingRow>, String> {
    let mut state = ProjectSettingsState::default();
    state.load(root)?;
    let rows = state.manifest().map(manifest_rows).unwrap_or_default();
    Ok(search_rows(&rows, query).into_iter().cloned().collect())
}

static UI_SCALE_REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Invalidation generation for explicit scale writes and their undo/redo.
pub(crate) fn ui_scale_revision() -> u64 {
    UI_SCALE_REVISION.load(std::sync::atomic::Ordering::Relaxed)
}

fn ui_scale_path(root: &Path) -> Result<PathBuf, RpcError> {
    crate::agent_ext::assets_request_path(root, "path", "ui/project.uisettings.ron")
}

fn scale_error(code: &str, message: impl Into<String>) -> RpcError {
    bad(
        code,
        message,
        "use editor.ui_scale_settings get; DPI points must increase and scales must be positive",
    )
}

fn scale_file(root: &Path) -> Result<regia_ui::scale::UiSettingsFile, RpcError> {
    crate::ui_designer_preview::read_scale_settings(&ui_scale_path(root)?)
        .map_err(|e| scale_error("ui_scale_read", e.to_string()))
}

fn scale_json(file: &regia_ui::scale::UiSettingsFile, applied: bool) -> Value {
    json!({ "path":"assets/ui/project.uisettings.ron", "version":file.version,
        "dpi_curve":file.dpi_curve, "scale_mode":file.scale_mode,
        "layer_budget_bytes":file.layer_budget_bytes, "applied":applied })
}

/// `curve` and `mode` are tri-state: `None` leaves the saved field, `Some(None)` clears it,
/// `Some(Some(_))` sets it. A set that sends neither is refused by the caller.
fn edit_ui_scale(
    root: &Path,
    curve: Option<Option<Vec<[f32; 2]>>>,
    mode: Option<Option<regia_ui::ScaleMode>>,
) -> Result<(regia_ui::scale::UiSettingsFile, InputConfigEdit), RpcError> {
    if let Some(Some(points)) = &curve {
        regia_ui::scale::DpiCurve {
            points: points.clone(),
        }
        .validate()
        .map_err(|e| scale_error("ui_scale_curve", e))?;
    }
    if let Some(Some(mode)) = &mode {
        regia_ui::validate_scale_mode(mode).map_err(|e| scale_error("ui_scale_curve", e))?;
    }
    let path = ui_scale_path(root)?;
    let mut file = scale_file(root)?;
    let before = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(scale_error("ui_scale_read", e.to_string())),
    };
    if let Some(points) = curve {
        file.dpi_curve = points;
    }
    if let Some(mode) = mode {
        file.scale_mode = mode;
    }
    let after = ron::ser::to_string_pretty(&file, regia_core::ron_pretty())
        .map_err(|e| scale_error("ui_scale_write", e.to_string()))?;
    // Validate the exact bytes the runtime will read before replacing the file.
    regia_ui::scale::parse_ui_settings_ron(&after, &path.display().to_string())
        .map_err(|e| scale_error("ui_scale_curve", e.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| scale_error("ui_scale_write", e.to_string()))?;
    }
    crate::level_io::write_atomic(&path, after.as_bytes()).map_err(|e| scale_error("ui_scale_write", e))?;
    UI_SCALE_REVISION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok((
        file,
        InputConfigEdit {
            path,
            before,
            after: Some(after),
            label: "UI scale".to_owned(),
        },
    ))
}

fn ui_scale_settings(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = project_root(world)?;
    let action = match params.get("action") {
        None => "get",
        Some(Value::String(value)) => value.as_str(),
        _ => return Err(scale_error("ui_scale_action", "action must be get or set")),
    };
    match action {
        "get" => Ok(scale_json(&scale_file(&root)?, false)),
        "set" => {
            let curve_value = params.get("dpi_curve");
            let mode_value = params.get("scale_mode");
            if curve_value.is_none() && mode_value.is_none() {
                return Err(scale_error("ui_scale_curve", "set needs dpi_curve or scale_mode"));
            }
            let curve = match curve_value {
                None => None,
                Some(value) => Some(
                    serde_json::from_value::<Option<Vec<[f32; 2]>>>(value.clone())
                        .map_err(|e| scale_error("ui_scale_curve", e.to_string()))?,
                ),
            };
            let mode = match mode_value {
                None => None,
                Some(Value::Null) => Some(None),
                Some(value) => Some(Some(
                    serde_json::from_value::<regia_ui::ScaleMode>(value.clone())
                        .map_err(|e| scale_error("ui_scale_curve", e.to_string()))?,
                )),
            };
            let (file, command) = edit_ui_scale(&root, curve, mode)?;
            push_input_undo(world, command);
            Ok(scale_json(&file, true))
        }
        _ => Err(scale_error("ui_scale_action", "action must be get or set")),
    }
}

#[derive(Clone)]
struct UiScaleDraft {
    curve: Option<Vec<[f32; 2]>>,
    mode: Option<regia_ui::ScaleMode>,
    message: String,
    frame: Option<u64>,
    revision: u64,
}

fn ui_scale_section(ui: &mut egui::Ui, root: &Path, undo: &mut Vec<InputConfigEdit>) {
    let id = egui::Id::new(("ui-scale-draft", root));
    let mut draft = ui
        .ctx()
        .data_mut(|d| d.get_temp::<UiScaleDraft>(id))
        .unwrap_or_else(|| match scale_file(root) {
            Ok(file) => UiScaleDraft {
                curve: file.dpi_curve,
                mode: file.scale_mode,
                message: String::new(),
                frame: None,
                revision: ui_scale_revision(),
            },
            Err(e) => UiScaleDraft {
                curve: None,
                mode: None,
                message: e.message,
                frame: None,
                revision: ui_scale_revision(),
            },
        });
    if draft.revision != ui_scale_revision() {
        match scale_file(root) {
            Ok(file) => {
                draft.curve = file.dpi_curve;
                draft.mode = file.scale_mode;
                draft.message.clear();
            }
            Err(e) => draft.message = e.message,
        }
        draft.revision = ui_scale_revision();
    }
    let frame = ui.ctx().cumulative_frame_nr();
    let fresh = draft.frame != Some(frame);
    ui.label("Display DPI → scale (physical size rule)");
    let before_curve = draft.curve.clone();
    let before_mode = draft.mode;
    ui_scale_curve_rows(ui, &mut draft.curve);
    ui_scale_mode_rows(ui, &mut draft.mode);
    if !fresh {
        draft.curve = before_curve;
        draft.mode = before_mode;
    }
    ui.horizontal(|ui| {
        if ui.button("Apply UI scale").clicked() && fresh {
            match edit_ui_scale(root, Some(draft.curve.clone()), Some(draft.mode)) {
                Ok((_, command)) => {
                    undo.push(command);
                    draft.message = "UI scale saved".into();
                }
                Err(e) => draft.message = e.message,
            }
        }
        if ui.button("Reload UI scale").clicked() && fresh {
            match scale_file(root) {
                Ok(file) => {
                    draft.curve = file.dpi_curve;
                    draft.mode = file.scale_mode;
                    draft.message.clear();
                }
                Err(e) => draft.message = e.message,
            }
        }
    });
    if !draft.message.is_empty() {
        ui.label(&draft.message);
    }
    draft.frame = Some(frame);
    draft.revision = ui_scale_revision();
    ui.ctx().data_mut(|d| d.insert_temp(id, draft));
}

/// Which project scale rule the Settings radios show. The engine default is the absent field.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ProjectScaleKind {
    Engine,
    Pixel,
    Screen,
    Physical,
    Integer,
}

fn scale_kind(mode: Option<regia_ui::ScaleMode>) -> ProjectScaleKind {
    match mode {
        None => ProjectScaleKind::Engine,
        Some(regia_ui::ScaleMode::ConstantPixelSize) => ProjectScaleKind::Pixel,
        Some(regia_ui::ScaleMode::ScaleWithScreen { .. }) => ProjectScaleKind::Screen,
        Some(regia_ui::ScaleMode::ConstantPhysicalSize) => ProjectScaleKind::Physical,
        Some(regia_ui::ScaleMode::IntegerScale { .. }) => ProjectScaleKind::Integer,
    }
}

fn mode_for_kind(
    kind: ProjectScaleKind,
    previous: Option<regia_ui::ScaleMode>,
) -> Option<regia_ui::ScaleMode> {
    match kind {
        ProjectScaleKind::Engine => None,
        ProjectScaleKind::Pixel => Some(regia_ui::ScaleMode::ConstantPixelSize),
        ProjectScaleKind::Physical => Some(regia_ui::ScaleMode::ConstantPhysicalSize),
        ProjectScaleKind::Screen => Some(match previous {
            Some(regia_ui::ScaleMode::ScaleWithScreen {
                reference,
                match_width,
            }) => regia_ui::ScaleMode::ScaleWithScreen {
                reference,
                match_width,
            },
            _ => regia_ui::ScaleMode::default(),
        }),
        ProjectScaleKind::Integer => Some(match previous {
            Some(mode @ regia_ui::ScaleMode::IntegerScale { .. }) => mode,
            _ => regia_ui::ScaleMode::IntegerScale {
                reference: [1920.0, 1080.0],
            },
        }),
    }
}

/// Project Settings radios for the saved scale rule, plus the reference numbers that rule needs.
fn ui_scale_mode_rows(ui: &mut egui::Ui, mode: &mut Option<regia_ui::ScaleMode>) {
    ui.label("Project scale rule");
    let mut kind = scale_kind(*mode);
    ui.horizontal(|ui| {
        ui.radio_value(&mut kind, ProjectScaleKind::Engine, "Engine default");
        let pixel = ui.radio_value(&mut kind, ProjectScaleKind::Pixel, "Constant pixel size");
        #[cfg(test)]
        ui.ctx()
            .data_mut(|d| d.insert_temp(egui::Id::new("ui-scale-mode"), pixel.id));
        // The integration-test build drops `cfg(test)`, so the id is still read there.
        #[cfg(not(test))]
        let _ = pixel.id;
        ui.radio_value(&mut kind, ProjectScaleKind::Screen, "Scale with screen");
        ui.radio_value(&mut kind, ProjectScaleKind::Physical, "Constant physical size");
        ui.radio_value(&mut kind, ProjectScaleKind::Integer, "Integer scale");
    });
    if kind != scale_kind(*mode) {
        *mode = mode_for_kind(kind, *mode);
    }
    match mode {
        Some(regia_ui::ScaleMode::ScaleWithScreen {
            reference,
            match_width,
        }) => {
            reference_drags(ui, reference);
            ui.label("Match width");
            let response = ui.add(egui::DragValue::new(match_width).speed(0.01).range(0.0..=1.0));
            crate::a11y_audit::named_drag_value(response, "Match width", f64::from(*match_width));
        }
        Some(regia_ui::ScaleMode::IntegerScale { reference }) => {
            reference_drags(ui, reference);
        }
        _ => {}
    }
}

fn reference_drags(ui: &mut egui::Ui, reference: &mut [f32; 2]) {
    ui.horizontal(|ui| {
        ui.label("Reference");
        let width = ui.add(
            egui::DragValue::new(&mut reference[0])
                .speed(1.0)
                .range(1.0..=8192.0),
        );
        crate::a11y_audit::named_drag_value(width, "Reference width", f64::from(reference[0]));
        let height = ui.add(
            egui::DragValue::new(&mut reference[1])
                .speed(1.0)
                .range(1.0..=8192.0),
        );
        crate::a11y_audit::named_drag_value(height, "Reference height", f64::from(reference[1]));
    });
}

fn ui_scale_curve_rows(ui: &mut egui::Ui, curve: &mut Option<Vec<[f32; 2]>>) {
    let _span = tracing::trace_span!("project_ui_scale_curve").entered();
    let mut enabled = curve.is_some();
    let response = ui.checkbox(&mut enabled, "Use DPI curve");
    #[cfg(test)]
    ui.ctx()
        .data_mut(|d| d.insert_temp(egui::Id::new("ui-scale-enable"), response.id));
    if response.changed() {
        *curve = enabled.then(|| vec![[96.0, 1.0]]);
    }
    let Some(points) = curve else {
        return;
    };
    let mut remove = None;
    egui::Grid::new("ui-scale-points").num_columns(3).show(ui, |ui| {
        ui.label("Display DPI");
        ui.label("Scale");
        ui.end_row();
        for (index, point) in points.iter_mut().enumerate() {
            ui.push_id(index, |ui| {
                for (axis, value) in point.iter_mut().enumerate() {
                    let response = ui.add(
                        egui::DragValue::new(&mut *value)
                            .speed(if axis == 0 { 1.0 } else { 0.05 })
                            .range(0.01..=16384.0),
                    );
                    crate::a11y_audit::named_drag_value(
                        response,
                        if axis == 0 { "Display DPI" } else { "UI scale" },
                        f64::from(*value),
                    );
                }
                if ui.button("Remove point").clicked() {
                    remove = Some(index);
                }
                ui.end_row();
            });
        }
    });
    if let Some(index) = remove {
        points.remove(index);
    }
    if ui.button("Add DPI point").clicked() {
        let [dpi, scale] = points.last().copied().unwrap_or([48.0, 1.0]);
        points.push([dpi + 48.0, scale]);
    }
    if let Err(message) = (regia_ui::scale::DpiCurve {
        points: points.clone(),
    })
    .validate()
    {
        ui.colored_label(crate::theme::ui_tokens(ui.ctx()).error, message);
    }
    let height = ui.spacing().interact_size.y * 4.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::hover());
    let tokens = crate::theme::ui_tokens(ui.ctx());
    ui.painter().rect_filled(rect, 0.0, tokens.well);
    let max_x = points.iter().map(|p| p[0]).fold(1.0, f32::max);
    let max_y = points.iter().map(|p| p[1]).fold(1.0, f32::max);
    let at = |p: [f32; 2]| {
        egui::pos2(
            rect.left() + rect.width() * p[0] / max_x,
            rect.bottom() - rect.height() * p[1] / max_y,
        )
    };
    for pair in points.windows(2) {
        ui.painter()
            .line_segment([at(pair[0]), at(pair[1])], egui::Stroke::new(1.5, tokens.signal));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A hand-written `config/input.ron` with comments, modifiers, triggers, two priorities, a
    /// consuming context and a SpaceMouse section.
    const INPUT_FIXTURE: &str = r#"// Player input
(
    actions: { "Jump": Button, "Look": Axis2D, "Sprint": Button, "Menu": Button },
    contexts: {
        // The game
        "Gameplay": (
            priority: 0,
            consume_lower: false,
            bindings: [
                (action: "Jump", input: Key("Space"), triggers: [Pressed]),
                (action: "Look", input: MouseDelta, modifiers: [Scale((2.0, 2.0, 1.0)), InvertY(true)]),
                (action: "Sprint", input: Key("ShiftLeft"), triggers: [Hold(0.25)]),
            ],
        ),
        "Menu": (
            priority: 10,
            consume_lower: true,
            bindings: [(action: "Menu", input: Key("Space"))],
        ),
    },
    spacemouse: (sensitivity: 2.0),
)
"#;

    fn project_with_input(label: &str, text: &str) -> PathBuf {
        let dir = regia_test_utils::unique_temp_dir(label);
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::fs::write(dir.join("config/input.ron"), text).unwrap();
        dir
    }

    fn read_config(dir: &Path) -> regia_input::InputConfig {
        regia_input::InputConfig::from_ron(&std::fs::read_to_string(dir.join("config/input.ron")).unwrap())
            .unwrap()
    }

    #[test]
    fn a_no_op_save_keeps_every_modifier_trigger_priority_and_loses_only_comments() {
        let dir = project_with_input("input-roundtrip", INPUT_FIXTURE);
        let before = read_config(&dir);
        assert_eq!(before.contexts["Menu"].priority, 10);
        // Setting a context to what it already is rewrites the file without changing the data.
        let edit = [InputEdit::SetContext {
            context: "Menu".into(),
            priority: Some(10),
            consume_lower: Some(true),
        }];
        let (summary, _) = edit_input_command(&dir, &edit).unwrap();
        assert_eq!(read_config(&dir), before, "data is unchanged by a no-op write");
        let text = std::fs::read_to_string(dir.join("config/input.ron")).unwrap();
        assert!(!text.contains("//"), "comments are not kept (documented): {text}");
        assert_eq!(before.spacemouse.sensitivity, 2.0);
        assert_eq!(read_config(&dir).spacemouse, before.spacemouse);
        // The summary shows modifiers and triggers as RON lists.
        let gameplay = summary.contexts.iter().find(|c| c.name == "Gameplay").unwrap();
        assert_eq!(gameplay.triggers[0], "[Pressed]");
        assert_eq!(gameplay.triggers[1], "[]");
        assert!(
            gameplay.modifiers[1].contains("Scale") && gameplay.modifiers[1].contains("InvertY"),
            "{:?}",
            gameplay.modifiers
        );
        assert_eq!(gameplay.modifiers[0], "[]");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn modifiers_triggers_and_priorities_edit_in_place() {
        let dir = project_with_input("input-edit", INPUT_FIXTURE);
        let edits = [
            InputEdit::SetModifiers {
                context: "Gameplay".into(),
                index: 1,
                modifiers: "[Scale((0.5, 0.5, 1.0))]".into(),
            },
            InputEdit::SetTriggers {
                context: "Gameplay".into(),
                index: 0,
                triggers: "[Pressed, Tap(0.2)]".into(),
            },
            InputEdit::SetTriggers {
                context: "Gameplay".into(),
                index: 2,
                triggers: "[]".into(),
            },
            InputEdit::SetContext {
                context: "Menu".into(),
                priority: Some(3),
                consume_lower: Some(false),
            },
            InputEdit::SetContext {
                context: "Cutscene".into(),
                priority: Some(20),
                consume_lower: None,
            },
        ];
        edit_input(&dir, &edits).unwrap();
        let config = read_config(&dir);
        let gameplay = &config.contexts["Gameplay"];
        assert_eq!(gameplay.bindings[1].modifiers.len(), 1);
        assert_eq!(
            gameplay.bindings[0].triggers,
            [regia_input::Trigger::Pressed, regia_input::Trigger::Tap(0.2)]
        );
        assert!(gameplay.bindings[2].triggers.is_empty(), "[] clears");
        assert_eq!(
            (
                config.contexts["Menu"].priority,
                config.contexts["Menu"].consume_lower
            ),
            (3, false)
        );
        assert_eq!(
            config.contexts["Cutscene"].priority, 20,
            "a missing context is created"
        );
        assert_eq!(config.spacemouse.sensitivity, 2.0, "other sections are kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refused_edits_write_nothing() {
        let dir = project_with_input("input-refused", INPUT_FIXTURE);
        let original = std::fs::read_to_string(dir.join("config/input.ron")).unwrap();
        let bad = |edit: InputEdit| edit_input(&dir, &[edit]).unwrap_err().code;
        let set_triggers = |index: usize, triggers: &str| InputEdit::SetTriggers {
            context: "Gameplay".into(),
            index,
            triggers: triggers.into(),
        };
        assert_eq!(bad(set_triggers(0, "[Hold(")), "bad_input_triggers");
        assert_eq!(bad(set_triggers(0, "[Nope]")), "bad_input_triggers");
        assert_eq!(bad(set_triggers(0, "[Hold(-1.0)]")), "bad_input_edit");
        assert_eq!(bad(set_triggers(0, "[Hold(0.0)]")), "bad_input_edit");
        assert_eq!(bad(set_triggers(0, "[Chord(\"Ghost\")]")), "bad_input_edit");
        assert_eq!(
            bad(set_triggers(0, "[Chord(\"Jump\")]")),
            "bad_input_edit",
            "its own action"
        );
        assert_eq!(bad(set_triggers(9, "[]")), "bad_input_edit", "no such binding");
        assert_eq!(
            bad(InputEdit::SetModifiers {
                context: "Nowhere".into(),
                index: 0,
                modifiers: "[]".into()
            }),
            "bad_input_edit"
        );
        assert_eq!(
            bad(InputEdit::SetModifiers {
                context: "Gameplay".into(),
                index: 1,
                modifiers: "[Smooth(-1.0)]".into()
            }),
            "bad_input_edit"
        );
        assert_eq!(
            bad(InputEdit::SetModifiers {
                context: "Gameplay".into(),
                index: 1,
                modifiers: "[DeadZone((mode: Axial, lo: inf, hi: NaN))]".into()
            }),
            "bad_input_edit",
            "a dead zone's edges must be finite"
        );
        assert_eq!(
            bad(InputEdit::SetModifiers {
                context: "Gameplay".into(),
                index: 1,
                modifiers: "Scale".into()
            }),
            "bad_input_modifiers"
        );
        assert_eq!(
            bad(InputEdit::SetContext {
                context: "Menu".into(),
                priority: None,
                consume_lower: None
            }),
            "bad_input_edit"
        );
        // A valid edit followed by a refused one is all or nothing.
        let mixed = [
            InputEdit::SetContext {
                context: "Menu".into(),
                priority: Some(99),
                consume_lower: None,
            },
            set_triggers(0, "[Hold(-5.0)]"),
        ];
        assert!(edit_input(&dir, &mixed).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join("config/input.ron")).unwrap(),
            original
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_input_box_follows_the_file_when_it_changes_underneath() {
        let mut draft = InputDraft::default();
        assert_eq!(draft.sync_box("Gameplay#0#t", "[Pressed]"), "[Pressed]");
        // The author types; the file has not changed: the typing stays.
        *draft.sync_box("Gameplay#0#t", "[Pressed]") = "[Released]".to_owned();
        assert_eq!(draft.sync_box("Gameplay#0#t", "[Pressed]"), "[Released]");
        // An undo, an agent edit or an index shift changes the file's value: the box follows it,
        // so "Set" cannot write the stale text back.
        assert_eq!(draft.sync_box("Gameplay#0#t", "[Hold(0.5)]"), "[Hold(0.5)]");
        assert_eq!(draft.boxes["Gameplay#0#t"], "[Hold(0.5)]");
    }

    #[test]
    fn a_consuming_context_hides_an_equal_priority_context_named_after_it() {
        // The runtime breaks priority ties by name, so "A" (consuming) runs before "B" and B's
        // Jump on Space never fires. The old `priority <=` skip reported nothing.
        let config = regia_input::InputConfig::from_ron(
            r#"(
                actions: { "Jump": Button, "Menu": Button },
                contexts: {
                    "A": (priority: 0, consume_lower: true, bindings: [(action: "Menu", input: Key("Space"))]),
                    "B": (priority: 0, consume_lower: true, bindings: [(action: "Jump", input: Key("Space"))]),
                },
            )"#,
        )
        .expect("config");
        let conflicts = input_conflicts(&config);
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert!(
            conflicts[0].starts_with("context A ") && conflicts[0].contains("hides B's binding"),
            "B (later name) cannot hide A: {conflicts:?}"
        );
    }

    #[test]
    fn conflicts_are_found_and_cleared_by_editing() {
        let dir = project_with_input("input-conflicts", INPUT_FIXTURE);
        // The fixture: Menu (priority 10, consuming) binds Space, and Gameplay's Jump is on Space.
        let summary = input_summary(&dir).unwrap().unwrap();
        assert_eq!(summary.conflicts.len(), 1, "{:?}", summary.conflicts);
        assert!(
            summary.conflicts[0].contains("Menu")
                && summary.conflicts[0].contains("hides")
                && summary.conflicts[0].contains("Jump"),
            "{:?}",
            summary.conflicts
        );
        // Lowering the Menu below Gameplay, or not consuming, clears it.
        let (summary, _) = edit_input_command(
            &dir,
            &[InputEdit::SetContext {
                context: "Menu".into(),
                priority: None,
                consume_lower: Some(false),
            }],
        )
        .unwrap();
        assert!(summary.conflicts.is_empty(), "{:?}", summary.conflicts);
        // Two actions on one key in one context with the same (default) trigger fight; a different
        // trigger does not; the same action twice is a duplicate.
        let add = |action: &str| InputEdit::AddBinding {
            context: "Gameplay".into(),
            action: action.into(),
            input: "Key(\"Space\")".into(),
        };
        let (summary, _) = edit_input_command(&dir, &[add("Sprint")]).unwrap();
        assert!(
            summary.conflicts.is_empty(),
            "Jump is on Pressed, Sprint on Down: no shared trigger: {:?}",
            summary.conflicts
        );
        let (summary, _) = edit_input_command(&dir, &[add("Menu")]).unwrap();
        assert!(
            summary
                .conflicts
                .iter()
                .any(|c| c.contains("Sprint") && c.contains("Menu") && c.contains("same trigger")),
            "Menu and Sprint are both on Down: {:?}",
            summary.conflicts
        );
        let (summary, _) = edit_input_command(&dir, &[add("Menu")]).unwrap();
        assert!(
            summary.conflicts.iter().any(|c| c.contains("twice")),
            "{:?}",
            summary.conflicts
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_input_edit_writes_nothing_and_adds_no_undo_step() {
        let dir = project_with_input("input-empty-edit", INPUT_FIXTURE);
        let original = std::fs::read(dir.join("config/input.ron")).unwrap();
        let mut world = World::new();
        world.insert_resource(crate::command::UndoStack::new());
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(dir.clone()),
        });
        let answer = project_input_edit(&mut world, &json!({ "edits": [] })).unwrap();
        assert!(answer["input"]["actions"].is_array(), "{answer}");
        assert_eq!(
            std::fs::read(dir.join("config/input.ron")).unwrap(),
            original,
            "the comments survive: no rewrite"
        );
        assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), 0);
        // A project without the file does not get an empty one.
        let fresh = regia_test_utils::unique_temp_dir("input-empty-edit-fresh");
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(fresh.clone()),
        });
        project_input_edit(&mut world, &json!({ "edits": [] })).unwrap();
        assert!(!fresh.join("config/input.ron").exists());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&fresh);
    }

    #[test]
    fn input_writes_undo_and_redo_through_the_editor_stack() {
        let dir = project_with_input("input-undo", INPUT_FIXTURE);
        let original = std::fs::read_to_string(dir.join("config/input.ron")).unwrap();
        let mut world = World::new();
        world.insert_resource(crate::command::UndoStack::new());
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(dir.clone()),
        });
        project_input_edit(
            &mut world,
            &json!({ "edits": [
                { "op": "set_context", "context": "Menu", "priority": 1 },
                { "op": "set_triggers", "context": "Gameplay", "index": 0, "triggers": "[Released]" },
            ] }),
        )
        .unwrap();
        assert_eq!(read_config(&dir).contexts["Menu"].priority, 1);
        world.resource_scope(|world, mut stack: Mut<crate::command::UndoStack>| {
            assert_eq!(stack.undo_depth(), 1, "one entry for one call");
            assert!(stack.undo(world));
        });
        assert_eq!(
            std::fs::read_to_string(dir.join("config/input.ron")).unwrap(),
            original,
            "byte for byte"
        );
        world.resource_scope(|world, mut stack: Mut<crate::command::UndoStack>| {
            assert!(stack.redo(world));
        });
        assert_eq!(read_config(&dir).contexts["Menu"].priority, 1);
        // A first write that creates the file is undone by removing it.
        let fresh = regia_test_utils::unique_temp_dir("input-undo-fresh");
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(fresh.clone()),
        });
        project_input_edit(
            &mut world,
            &json!({ "edits": [{ "op": "add_action", "name": "Jump", "kind": "Button" }] }),
        )
        .unwrap();
        let created = std::fs::read_to_string(fresh.join("config/input.ron")).expect("created input map");
        assert!(
            created.contains("Jump"),
            "add_action writes the action into config/input.ron: {created}"
        );
        world.resource_scope(|world, mut stack: Mut<crate::command::UndoStack>| {
            assert!(stack.undo(world));
        });
        assert!(!fresh.join("config/input.ron").exists());
        // Bad ops give the documented codes.
        let e = project_input_edit(
            &mut world,
            &json!({ "edits": [{ "op": "set_triggers", "context": "G", "index": 0, "triggers": "[" }] }),
        )
        .unwrap_err();
        assert!(
            e.data
                .as_ref()
                .is_some_and(|d| d["code"] == "bad_input_edit" || d["code"] == "bad_input_triggers"),
            "{e:?}"
        );
        let e = project_input_edit(
            &mut world,
            &json!({ "edits": [{ "op": "set_context", "context": "G", "priority": "high" }] }),
        )
        .unwrap_err();
        assert_eq!(
            e.data.as_ref().map(|d| d["code"].clone()),
            Some(json!("bad_input_edit"))
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&fresh);
    }

    #[test]
    fn an_input_edit_leaves_a_saved_level_clean() {
        // The input map has its own file: editing it must not flip the level's dirty state
        // (which drives the close prompt and the autosave recovery copy), even though the edit
        // stays undoable.
        let dir = project_with_input("input-dirty", INPUT_FIXTURE);
        let mut world = World::new();
        world.insert_resource(crate::command::UndoStack::new());
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(dir.clone()),
        });
        assert!(
            !world.resource::<crate::command::UndoStack>().is_dirty(),
            "a fresh stack is saved"
        );
        project_input_edit(
            &mut world,
            &json!({ "edits": [{ "op": "set_context", "context": "Menu", "priority": 7 }] }),
        )
        .unwrap();
        assert_eq!(read_config(&dir).contexts["Menu"].priority, 7);
        assert!(
            !world.resource::<crate::command::UndoStack>().is_dirty(),
            "an input edit does not dirty the level"
        );
        // Undo and redo rewrite the file but still leave the level clean.
        world.resource_scope(|world, mut stack: Mut<crate::command::UndoStack>| {
            assert_eq!(stack.undo_depth(), 1, "the edit is undoable");
            assert!(stack.undo(world));
            assert!(!stack.is_dirty(), "undoing it does not dirty the level either");
        });
        assert_eq!(read_config(&dir).contexts["Menu"].priority, 10, "file restored");
        world.resource_scope(|world, mut stack: Mut<crate::command::UndoStack>| {
            assert!(stack.redo(world));
            assert!(!stack.is_dirty(), "redoing it is still level-neutral");
        });
        assert_eq!(read_config(&dir).contexts["Menu"].priority, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_priority_drag_commits_once_when_it_stops() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
        let context = ContextSummary {
            name: "Gameplay".to_owned(),
            priority: 0,
            consume_lower: false,
            bindings: Vec::new(),
            modifiers: Vec::new(),
            triggers: Vec::new(),
        };
        // A probe paints the same row the helper does, to learn where the DragValue sits (the
        // helper itself does not expose it).
        let mut at = None;
        let mut probe = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen),
                ..egui::RawInput::default()
            },
            |ui| {
                ui.horizontal(|ui| {
                    ui.label("Priority");
                    let mut p = 0i32;
                    at = Some(ui.add(egui::DragValue::new(&mut p)).rect);
                    let mut c = false;
                    let _ = ui.checkbox(&mut c, "Consumes lower");
                });
            },
        );
        probe.textures_delta.clear();
        let at = at.expect("the probe row has a drag value").center();
        let frame = |ctx: &egui::Context, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(screen),
                events,
                ..egui::RawInput::default()
            };
            let mut edits = Vec::new();
            let mut output = ctx.run_ui(input, |ui| {
                ui.horizontal(|ui| context_priority_row(ui, &context, &mut edits));
            });
            output.textures_delta.clear();
            edits
        };
        frame(&ctx, vec![]);
        frame(&ctx, vec![egui::Event::PointerMoved(at)]);
        let press = |pos: egui::Pos2, pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        assert!(frame(&ctx, vec![press(at, true)]).is_empty());
        let mut during = 0;
        for step in 1..=10 {
            let pos = at + egui::vec2(step as f32 * 3.0, 0.0);
            during += frame(&ctx, vec![egui::Event::PointerMoved(pos)]).len();
        }
        assert_eq!(during, 0, "no commit while the drag is in progress");
        let end = at + egui::vec2(30.0, 0.0);
        let committed = frame(&ctx, vec![press(end, false)]);
        assert_eq!(
            committed.len(),
            1,
            "one commit when the drag stops: {committed:?}"
        );
        match &committed[0] {
            InputEdit::SetContext {
                context,
                priority: Some(priority),
                consume_lower,
            } => {
                assert_eq!(context, "Gameplay");
                assert_eq!(*consume_lower, None);
                assert!(*priority > 0, "the dragged value, not the file's 0: {priority}");
            }
            other => panic!("expected one SetContext edit, got {other:?}"),
        }
    }

    #[test]
    fn render_profiles_validate_by_name() {
        let table: toml::Table = toml::from_str(
            "[project]\nname='a'\n[platforms.android]\nrender_profile='Nope'\n[platforms.amiga]\n",
        )
        .unwrap();
        let p = problems(&table);
        assert_eq!(p.len(), 2, "{p:?}");
        assert!(
            p.iter()
                .any(|x| x.key == "platforms.android.render_profile" && x.blocks_save)
        );
        assert!(p.iter().any(|x| x.key == "platforms.amiga" && !x.blocks_save));
    }

    /// ADR-8844: the project-settings check and picker take the integrated-GPU floor by name;
    /// `Low` (a row's tier, no name of its own) is still flagged.
    #[test]
    fn render_profiles_accept_very_low_and_flag_low() {
        let table: toml::Table = toml::from_str(
            "[project]\nname='a'\n[platforms.windows]\nrender_profile='VeryLow'\n[platforms.linux]\nrender_profile='Low'\n",
        )
        .unwrap();
        let p = problems(&table);
        assert!(
            !p.iter().any(|x| x.key == "platforms.windows.render_profile"),
            "{p:?}"
        );
        let low = p
            .iter()
            .find(|x| x.key == "platforms.linux.render_profile")
            .expect("Low is flagged");
        assert!(low.blocks_save && low.message.contains("VeryLow"), "{low:?}");
        let names = render_profile_names();
        assert!(names.contains(&"VeryLow") && names.contains(&"Medium") && !names.contains(&"Low"));

        // The picker writes the canonical name.
        let dir = regia_test_utils::unique_temp_dir("project-settings-very-low");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Regia.toml"),
            "[project]\nname = 'a'\nid = 'com.example.a'\n",
        )
        .unwrap();
        let mut state = ProjectSettingsState::default();
        state.load(&dir).unwrap();
        state.set_render_profile("windows", Some("very_low")).unwrap();
        assert_eq!(state.render_profile("windows").as_deref(), Some("VeryLow"));
        let err = state.set_render_profile("windows", Some("Low")).unwrap_err();
        assert!(err.contains("VeryLow"), "{err}");
        assert_eq!(state.render_profile("windows").as_deref(), Some("VeryLow"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn startup_level_must_stay_under_assets_and_be_a_level() {
        for (level, bad) in [
            ("levels/a.level.ron", false),
            ("../x.level.ron", true),
            ("/abs.level.ron", true),
            ("levels/a.png", true),
        ] {
            let table: toml::Table =
                toml::from_str(&format!("[project]\nname='a'\nstartup_level={level:?}\n")).unwrap();
            assert_eq!(!problems(&table).is_empty(), bad, "{level}");
        }
    }

    #[test]
    fn settings_search_finds_nested_manifest_keys() {
        let table: toml::Table =
            toml::from_str("[project]\nname = 'Demo'\n[platforms.android]\nrender_profile = 'mobile'\n")
                .expect("toml");
        let rows = manifest_rows(&table);
        assert_eq!(
            rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            ["platforms.android.render_profile", "project.name"]
        );
        let hits = search_rows(&rows, "android PROFILE");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].value, "\"mobile\"");
        assert_eq!(search_rows(&rows, "").len(), 2);
        assert!(search_rows(&rows, "nothing").is_empty());
    }
}

#[cfg(test)]
mod ui_scale_tests {
    use super::*;

    #[test]
    fn ui_scale_settings_dispatch_preserves_budget_and_rejects_bad_curve() {
        let root = regia_test_utils::unique_temp_dir("w12-ui-scale");
        std::fs::create_dir_all(root.join("assets/ui")).expect("valid test fixture");
        regia_test_utils::write_atomic(
            &root.join("assets/ui/project.uisettings.ron"),
            b"(version:1,layer_budget_bytes:123456)",
        )
        .expect("valid test fixture");
        let original =
            std::fs::read(root.join("assets/ui/project.uisettings.ron")).expect("valid test fixture");
        let mut world = World::new();
        world.init_resource::<crate::command::UndoStack>();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let methods = methods();
        let (_, handler) = methods
            .iter()
            .find(|(s, _)| s.name == "editor.ui_scale_settings")
            .expect("UI scale method");
        let out = handler(&mut world, &json!({"action":"set", "dpi_curve":[[96,1],[192,2]]}))
            .expect("valid test fixture");
        assert_eq!(out["layer_budget_bytes"], 123456);
        let path = root.join("assets/ui/project.uisettings.ron");
        let before = std::fs::read(&path).expect("valid test fixture");
        assert!(handler(&mut world, &json!({"action":"set", "dpi_curve":[[192,2],[96,1]]})).is_err());
        assert_eq!(std::fs::read(&path).expect("valid test fixture"), before);
        assert_eq!(
            handler(&mut world, &json!({"action":"get"})).expect("valid test fixture")["dpi_curve"],
            json!([[96.0, 1.0], [192.0, 2.0]])
        );
        assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), 1);
        let revision = ui_scale_revision();
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(
            std::fs::read(&path).expect("valid test fixture"),
            original,
            "undo restores exact source bytes"
        );
        assert_ne!(ui_scale_revision(), revision, "undo invalidates cached previews");
        assert!(crate::command::redo_world(&mut world));
        assert_eq!(std::fs::read(&path).expect("valid test fixture"), before);
        let cleared =
            handler(&mut world, &json!({"action":"set", "dpi_curve":null})).expect("valid test fixture");
        assert!(cleared["dpi_curve"].is_null());
        assert_eq!(cleared["layer_budget_bytes"], 123456);
        std::fs::remove_dir_all(root).expect("valid test fixture");
    }
    #[test]
    fn ui_scale_curve_controls_accept_keyboard_input() {
        let ctx = egui::Context::default();
        let mut curve = None;
        ctx.run_ui(egui::RawInput::default(), |ui| {
            ui_scale_curve_rows(ui, &mut curve);
            let id = ui
                .ctx()
                .data(|d| d.get_temp::<egui::Id>(egui::Id::new("ui-scale-enable")))
                .expect("valid test fixture");
            let _ = id;
        })
        .textures_delta
        .clear();
        let target = ctx
            .data(|d| d.get_temp::<egui::Id>(egui::Id::new("ui-scale-enable")))
            .expect("valid test fixture");
        for _ in 0..4 {
            if ctx.memory(|m| m.focused()) == Some(target) {
                break;
            }
            ctx.run_ui(
                egui::RawInput {
                    events: [true, false]
                        .into_iter()
                        .map(|pressed| egui::Event::Key {
                            key: egui::Key::Tab,
                            physical_key: None,
                            pressed,
                            repeat: false,
                            modifiers: egui::Modifiers::NONE,
                        })
                        .collect(),
                    ..Default::default()
                },
                |ui| ui_scale_curve_rows(ui, &mut curve),
            )
            .textures_delta
            .clear();
        }
        assert_eq!(
            ctx.memory(|m| m.focused()),
            Some(target),
            "DPI curve checkbox is in the Tab order"
        );
        ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Space,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| ui_scale_curve_rows(ui, &mut curve),
        )
        .textures_delta
        .clear();
        assert_eq!(curve, Some(vec![[96.0, 1.0]]));
    }

    #[test]
    fn the_saved_scale_rule_is_keyboard_operable_and_round_trips() {
        let ctx = egui::Context::default();
        let mut mode = None;
        ctx.run_ui(egui::RawInput::default(), |ui| {
            ui_scale_mode_rows(ui, &mut mode);
        })
        .textures_delta
        .clear();
        let target = ctx
            .data(|d| d.get_temp::<egui::Id>(egui::Id::new("ui-scale-mode")))
            .expect("valid test fixture");
        for _ in 0..8 {
            if ctx.memory(|m| m.focused()) == Some(target) {
                break;
            }
            ctx.run_ui(
                egui::RawInput {
                    events: [true, false]
                        .into_iter()
                        .map(|pressed| egui::Event::Key {
                            key: egui::Key::Tab,
                            physical_key: None,
                            pressed,
                            repeat: false,
                            modifiers: egui::Modifiers::NONE,
                        })
                        .collect(),
                    ..Default::default()
                },
                |ui| ui_scale_mode_rows(ui, &mut mode),
            )
            .textures_delta
            .clear();
        }
        assert_eq!(
            ctx.memory(|m| m.focused()),
            Some(target),
            "Constant pixel size is in the Tab order"
        );
        ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Space,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| ui_scale_mode_rows(ui, &mut mode),
        )
        .textures_delta
        .clear();
        assert_eq!(mode, Some(regia_ui::ScaleMode::ConstantPixelSize));

        let root = regia_test_utils::unique_temp_dir("w13-ui-scale-mode");
        std::fs::create_dir_all(root.join("assets/ui")).expect("valid test fixture");
        regia_test_utils::write_atomic(
            &root.join("assets/ui/project.uisettings.ron"),
            b"(version:1,dpi_curve:[(96.0,1.0),(192.0,2.0)],layer_budget_bytes:99)",
        )
        .expect("valid test fixture");
        let mut world = World::new();
        world.init_resource::<crate::command::UndoStack>();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let methods = methods();
        let (_, handler) = methods
            .iter()
            .find(|(s, _)| s.name == "editor.ui_scale_settings")
            .expect("UI scale method");
        let saved = handler(
            &mut world,
            &json!({"action":"set","scale_mode":{"IntegerScale":{"reference":[320.0,180.0]}}}),
        )
        .expect("valid test fixture");
        assert_eq!(saved["dpi_curve"], json!([[96.0, 1.0], [192.0, 2.0]]));
        assert_eq!(saved["layer_budget_bytes"], 99);
        assert_eq!(
            saved["scale_mode"],
            json!({"IntegerScale":{"reference":[320.0,180.0]}})
        );
        let bad = handler(
            &mut world,
            &json!({"action":"set","scale_mode":{"IntegerScale":{"reference":[0.0,180.0]}}}),
        )
        .expect_err("a zero reference is refused");
        assert_eq!(
            bad.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(|code| code.as_str()),
            Some("ui_scale_curve")
        );
        let kept = handler(&mut world, &json!({"action":"get"})).expect("valid test fixture");
        assert_eq!(
            kept["scale_mode"], saved["scale_mode"],
            "a refused set writes nothing"
        );
        let cleared = handler(&mut world, &json!({"action":"set","scale_mode":null})).expect("clear");
        assert!(cleared["scale_mode"].is_null());
        assert_eq!(cleared["dpi_curve"], json!([[96.0, 1.0], [192.0, 2.0]]));
        let missing = handler(&mut world, &json!({"action":"set"})).expect_err("needs a field");
        assert_eq!(
            missing
                .data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(|code| code.as_str()),
            Some("ui_scale_curve")
        );
        std::fs::remove_dir_all(root).expect("valid test fixture");
    }
}

#[cfg(test)]
mod default_reset_tests {
    use super::*;

    fn call(world: &mut World, name: &str, params: Value) -> Value {
        if world
            .get_resource::<regia_agent::extension::AgentExtension>()
            .is_none()
        {
            crate::agent_ext::install(world);
        }
        let extension = world.resource::<regia_agent::extension::AgentExtension>().clone();
        let (_, handler) = extension.method(name).expect("registered method");
        handler(world, &params).expect("method succeeds")
    }

    #[test]
    fn a_changed_project_setting_is_marked_and_reverts_in_one_undo_step() {
        let root = regia_test_utils::unique_temp_dir("project-default-reset");
        std::fs::create_dir_all(&root).expect("root");
        let original = "# keep this comment on undo\n[project]\nname='Demo'\nid='com.example.demo'\nversion='2.0.0'\nbuild_number=42\n[custom]\nkeep=9\n";
        crate::level_io::write_atomic(&root.join("Regia.toml"), original.as_bytes()).expect("manifest");
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        world.insert_resource(crate::command::UndoStack::default());
        let get = call(&mut world, "editor.project_settings_get", json!({}));
        let rows = get["settings"].as_array().expect("default metadata");
        let version = rows
            .iter()
            .find(|row| row["key"] == "project.version")
            .expect("version");
        assert_eq!(version["changed"], true);
        assert_eq!(version["default"], "\"0.1.0\"");
        let unknown = rows
            .iter()
            .find(|row| row["key"] == "custom.keep")
            .expect("unknown key");
        assert_eq!(unknown["resettable"], false);
        assert!(unknown["changed"].is_null());
        let dry = call(
            &mut world,
            "editor.project_settings_set",
            json!({"reset_category": "project", "save": false}),
        );
        assert_eq!(dry["project"]["version"], "0.1.0");
        assert_eq!(
            std::fs::read_to_string(root.join("Regia.toml")).expect("dry run text"),
            original
        );
        assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), 0);
        let extension = world.resource::<regia_agent::extension::AgentExtension>().clone();
        let (_, handler) = extension
            .method("editor.project_settings_set")
            .expect("registered");
        let error = handler(
            &mut world,
            &json!({"reset_keys": ["project.version", "custom.keep"]}),
        )
        .expect_err("unknown default is refused atomically");
        assert_eq!(error.data.expect("coded error")["code"], "bad_settings");
        assert_eq!(
            std::fs::read_to_string(root.join("Regia.toml")).expect("refused text"),
            original
        );

        call(
            &mut world,
            "editor.project_settings_set",
            json!({"reset_keys": ["project.version"]}),
        );
        let get = call(&mut world, "editor.project_settings_get", json!({}));
        assert_eq!(get["project"]["version"], "0.1.0");
        assert_eq!(get["project"]["build_number"], 42);
        assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), 1);
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(
            std::fs::read_to_string(root.join("Regia.toml")).expect("undo text"),
            original
        );
        call(
            &mut world,
            "editor.project_settings_set",
            json!({"reset_category": "project"}),
        );
        let get = call(&mut world, "editor.project_settings_get", json!({}));
        assert_eq!(get["project"]["version"], "0.1.0");
        assert_eq!(get["project"]["build_number"], 1);
        assert_eq!(get["project"]["name"], "Demo");
        assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), 1);
        assert!(
            !world.resource::<crate::command::UndoStack>().is_dirty(),
            "settings do not dirty a level"
        );
        call(
            &mut world,
            "editor.project_settings_set",
            json!({"reset_category": "project"}),
        );
        assert_eq!(
            world.resource::<crate::command::UndoStack>().undo_depth(),
            1,
            "no-op reset adds no entry"
        );
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(
            std::fs::read_to_string(root.join("Regia.toml")).expect("undo text"),
            original
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
