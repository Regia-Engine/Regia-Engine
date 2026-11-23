// SPDX-License-Identifier: MPL-2.0
//! The Settings window (`docs/specs/19-editor.md` § 2 "Editor Preferences", ADR-2321, ADR-7767): the
//! Preferences tab (the typed settings of [`crate::editor_prefs`] with changed values marked, Reset
//! per key and per category, Export and Import), the Keymap tab of [`crate::editor_keymap`] (rebind,
//! scope, clear, conflicts, presets, "Press a key…" search, the keyboard map, import and export) and
//! the Project Settings tab (the `Regia.toml` keys). One search box serves all three tabs.
//!
//! Every control edits the same world resources the agent methods edit, and the shell writes the
//! files when they change, so the panel has no Save button. Every control is a focusable egui
//! widget (Tab moves, Enter or Space activates); colors come from the theme tokens.

use std::path::PathBuf;

use regia_ecs::prelude::*;

use crate::command_palette::{COMMANDS, CommandKeymap, KeyChord};
use crate::editor_keymap::{
    ChordCapture, ImportMode, KEYBOARD_ROWS, KeyState, KeymapStore, ShortcutScope, apply_preset,
    builtin_scope, conflicts, import_keymap, key_states, keymap_to_text, list_presets, lookup, presets_dir,
    save_preset,
};
use crate::editor_prefs::{EditorConfigPaths, EditorPreferences, PrefKind, PrefValue, PrefsStore};
use crate::project_settings_ext::{ProjectSettingRow, search_rows};

/// The panel's UI state.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct PrefsPanelState {
    /// The search text (shared by the three tabs).
    pub query: String,
    /// Whether the Keymap tab shows (else Preferences), unless [`Self::project_tab`].
    pub keymap_tab: bool,
    /// Whether the Project Settings tab shows (ADR-7767).
    pub project_tab: bool,
    /// Whether the Theme tab shows (wins over `keymap_tab`; ADR-7838).
    pub theme_tab: bool,
    /// The status line of the last action.
    pub status: String,
    /// The chord the Keymap tab lists the commands of ("Press a key…" or a keyboard map key).
    pub chord_filter: Option<KeyChord>,
    /// Whether the keyboard map shows.
    pub show_keyboard: bool,
    /// The keyboard map's modifier layer: Ctrl, Shift, Alt.
    pub map_modifiers: [bool; 3],
    /// The name typed for Save preset.
    pub preset_name: String,
    /// Whether Import replaces (else merges), for both the preferences and the keymap.
    pub import_replace: bool,
    /// The `Regia.toml` rows, read once per project (Reload reads again).
    pub project_rows: Vec<ProjectSettingRow>,
    /// The project the rows were read for.
    pub project_loaded_for: Option<PathBuf>,
    /// Why the manifest could not be read.
    pub project_error: Option<String>,
    /// The keymap presets, listed when the Keymap tab first draws and after a save.
    pub presets: Option<Vec<crate::editor_keymap::KeymapPreset>>,
}

impl PrefsPanelState {
    /// The project rows for `root`, reading the manifest when the project changed (not per frame).
    fn refresh_project(&mut self, root: Option<PathBuf>) {
        if self.project_loaded_for == root {
            return;
        }
        self.project_rows.clear();
        self.project_error = None;
        if let Some(root) = &root {
            match crate::project_settings_ext::search_project_settings(root, "") {
                Ok(rows) => self.project_rows = rows,
                Err(err) => self.project_error = Some(err),
            }
        }
        self.project_loaded_for = root;
    }

    /// The tab that shows: `settings`, `keymap`, `theme` or `project`.
    #[must_use]
    pub fn tab_name(&self) -> &'static str {
        if self.project_tab {
            "project"
        } else if self.theme_tab {
            "theme"
        } else if self.keymap_tab {
            "keymap"
        } else {
            "settings"
        }
    }

    /// Shows the tab `name` (`settings`, `keymap`, `theme` or `project`). Returns whether the name
    /// is one of those.
    pub fn set_tab(&mut self, name: &str) -> bool {
        let (keymap, theme, project) = match name {
            "settings" => (false, false, false),
            "keymap" => (true, false, false),
            "theme" => (false, true, false),
            "project" => (false, false, true),
            _ => return false,
        };
        (self.keymap_tab, self.theme_tab, self.project_tab) = (keymap, theme, project);
        true
    }
}

fn project_root(world: &World) -> Option<PathBuf> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
}

/// Draws the Settings window's tab.
#[tracing::instrument(target = "regia_editor", skip_all, name = "preferences_panel")]
pub fn preferences_panel(ui: &mut egui::Ui, world: &mut World) {
    let mut state = world.remove_resource::<PrefsPanelState>().unwrap_or_default();
    state.refresh_project(project_root(world));
    ui.horizontal(|ui| {
        for (name, label) in [
            ("settings", "Preferences"),
            ("keymap", "Keymap"),
            ("theme", "Theme"),
            ("project", "Project Settings"),
        ] {
            if ui.selectable_label(state.tab_name() == name, label).clicked() {
                state.set_tab(name);
            }
        }
        ui.separator();
        let search = ui.add(
            egui::TextEdit::singleline(&mut state.query)
                .hint_text("Search preferences, shortcuts and project settings")
                .desired_width(260.0),
        );
        crate::a11y_audit::name(search, egui::WidgetType::TextEdit, "Search preferences");
    });
    ui.separator();
    if state.project_tab {
        project_tab(ui, world, &mut state);
    } else if state.theme_tab {
        theme_tab(ui, world, &mut state);
    } else if state.keymap_tab {
        keymap_tab(ui, world, &mut state);
    } else {
        settings_tab(ui, world, &mut state);
    }
    if !state.status.is_empty() {
        ui.separator();
        ui.weak(&state.status);
    }
    world.insert_resource(state);
}

/// The other tab's matches for the shared search, with a button to switch to it.
fn other_matches(ui: &mut egui::Ui, state: &mut PrefsPanelState) {
    if state.query.trim().is_empty() {
        return;
    }
    let project = search_rows(&state.project_rows, &state.query).len();
    let prefs = EditorPreferences::search(&state.query).len();
    ui.horizontal(|ui| {
        if state.project_tab {
            ui.weak(format!("{prefs} matching preferences"));
            if prefs > 0 && ui.small_button("Show preferences").clicked() {
                state.project_tab = false;
                state.keymap_tab = false;
            }
        } else {
            ui.weak(format!("{project} matching project settings"));
            if project > 0 && ui.small_button("Show project settings").clicked() {
                state.project_tab = true;
            }
        }
    });
}

fn settings_tab(ui: &mut egui::Ui, world: &mut World, state: &mut PrefsPanelState) {
    let mut prefs = world
        .get_resource_or_insert_with(EditorPreferences::default)
        .clone();
    let before = prefs.generation();
    let store = world.get_resource::<PrefsStore>().cloned().unwrap_or_default();
    if let Some(reason) = &store.read_only {
        ui.colored_label(
            crate::theme::ui_tokens(ui.ctx()).warn,
            format!("Not saved: {reason}"),
        );
    }
    for line in &store.diagnostics {
        ui.weak(line);
    }
    other_matches(ui, state);
    let rows = EditorPreferences::search(&state.query);
    if rows.is_empty() {
        ui.weak("No preference matches");
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        let mut category = "";
        for def in rows {
            if def.category != category {
                category = def.category;
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.strong(category);
                    let changed = crate::editor_prefs::PREFS
                        .iter()
                        .any(|d| d.category == category && !prefs.is_default(d.key));
                    if changed && ui.small_button("Reset category").clicked() {
                        match prefs.reset_category(category) {
                            Ok(keys) => state.status = format!("{category}: {} reset", keys.len()),
                            Err(err) => state.status = err.to_string(),
                        }
                    }
                });
            }
            ui.horizontal(|ui| {
                let value = prefs.get(def.key).unwrap_or_else(|| def.default.value());
                let mut next = None;
                let tip = crate::tooltips::RichTooltip::for_pref(def);
                // A changed value: a dot and a bold label (ADR-7767).
                let label = if prefs.is_default(def.key) {
                    ui.label(def.label)
                } else {
                    ui.strong(format!("● {}", def.label))
                };
                crate::tooltips::attach(label, &tip);
                match (def.kind, value) {
                    (PrefKind::Bool, PrefValue::Bool(mut b)) => {
                        if crate::a11y_audit::bare_checkbox(ui, &mut b, def.label).changed() {
                            next = Some(PrefValue::Bool(b));
                        }
                    }
                    (PrefKind::Int { min, max }, PrefValue::Int(mut i)) => {
                        let r = ui.add(egui::DragValue::new(&mut i).range(min..=max));
                        if crate::a11y_audit::name(r, egui::WidgetType::DragValue, def.label).changed() {
                            next = Some(PrefValue::Int(i));
                        }
                    }
                    (PrefKind::Float { min, max }, PrefValue::Float(mut f)) => {
                        let r = ui.add(egui::DragValue::new(&mut f).range(min..=max).speed(0.05));
                        if crate::a11y_audit::name(r, egui::WidgetType::DragValue, def.label).changed() {
                            next = Some(PrefValue::Float(f));
                        }
                    }
                    (PrefKind::Choice(choices), PrefValue::Text(t)) => {
                        let combo = egui::ComboBox::from_id_salt(("pref", def.key))
                            .selected_text(t.clone())
                            .show_ui(ui, |ui| {
                                for c in choices {
                                    if ui.selectable_label(t == *c, *c).clicked() {
                                        next = Some(PrefValue::Text((*c).to_owned()));
                                    }
                                }
                            });
                        crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, def.label);
                    }
                    (PrefKind::Text { .. }, PrefValue::Text(mut t)) => {
                        let r = ui.text_edit_singleline(&mut t);
                        if crate::a11y_audit::name(r, egui::WidgetType::TextEdit, def.label).changed() {
                            next = Some(PrefValue::Text(t));
                        }
                    }
                    _ => {
                        ui.weak("(unexpected value)");
                    }
                }
                if let Some(value) = next
                    && let Err(err) = prefs.set(def.key, &value)
                {
                    state.status = err.to_string();
                }
                if !prefs.is_default(def.key) && ui.small_button("Reset").clicked() {
                    let _ = prefs.reset(def.key);
                }
            });
        }
    });
    ui.separator();
    ui.horizontal(|ui| {
        if ui.button("Reset all to defaults").clicked() {
            let n = prefs.reset_all();
            state.status = format!("{n} preferences reset");
        }
        if ui.button("Export preferences…").clicked()
            && let Some(path) = crate::file_dialog::FileDialog::new()
                .add_filter("Preferences", &["ron"])
                .set_file_name("my.prefs.ron")
                .save_file()
        {
            state.status = match crate::level_io::write_atomic(&path, prefs.to_text().as_bytes()) {
                Ok(()) => format!("exported to {}", path.display()),
                Err(err) => format!("export failed: {err}"),
            };
        }
        if ui.button("Import preferences…").clicked()
            && let Some(path) = crate::file_dialog::FileDialog::new()
                .add_filter("Preferences", &["ron"])
                .pick_file()
        {
            let mode = import_mode(state.import_replace);
            match std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|text| prefs.import(&text, mode, true).map_err(|e| e.to_string()))
            {
                Ok(report) => {
                    state.status = format!(
                        "imported {} preferences ({} changed, {} problems)",
                        report.applied.len(),
                        report.changed.len(),
                        report.diagnostics.len()
                    );
                    prefs = report.prefs;
                }
                Err(err) => state.status = format!("import failed: {err}"),
            }
        }
        import_replace_toggle(ui, state);
    });
    if prefs.generation() != before {
        world.insert_resource(prefs);
    }
    // Typed user settings of extensions, under each extension's name (ADR-7791).
    crate::extension::settings::settings_section(
        ui,
        world,
        crate::extension::settings::SettingScope::User,
        &state.query,
    );
}

/// The "Import replaces" switch, drawn beside both tabs' Import buttons (one setting for both).
fn import_replace_toggle(ui: &mut egui::Ui, state: &mut PrefsPanelState) {
    crate::a11y_audit::bare_checkbox(ui, &mut state.import_replace, "Import replaces");
    ui.label("Import replaces");
}

fn import_mode(replace: bool) -> ImportMode {
    if replace {
        ImportMode::Replace
    } else {
        ImportMode::Merge
    }
}

fn keymap_tab(ui: &mut egui::Ui, world: &mut World, state: &mut PrefsPanelState) {
    // A chord captured by the palette's key handler since the last frame.
    let mut capture = world.get_resource::<ChordCapture>().copied().unwrap_or_default();
    if let Some(chord) = capture.captured.take() {
        state.chord_filter = Some(chord);
        world.insert_resource(capture);
    }
    presets_row(ui, world, state);
    let mut keymap = world.get_resource_or_insert_with(CommandKeymap::default).clone();
    let original = keymap.clone();
    let store = world.get_resource::<KeymapStore>().cloned().unwrap_or_default();
    if let Some(reason) = &store.read_only {
        ui.colored_label(
            crate::theme::ui_tokens(ui.ctx()).warn,
            format!("Not saved: {reason}"),
        );
    }
    for line in &store.problems {
        ui.weak(line);
    }
    ui.horizontal(|ui| {
        let text = if capture.active {
            "Press a key… (Esc cancels)"
        } else {
            "Press a key…"
        };
        let response = ui
            .button(text)
            .on_hover_text("Find every command a key chord runs, in any scope");
        let frame = ui.ctx().cumulative_frame_nr();
        if response.clicked() {
            world.insert_resource(ChordCapture {
                active: true,
                captured: None,
                armed_frame: frame,
            });
            response.request_focus();
        } else if capture.active {
            // Keep the capture alive while this tab draws it, and hold the keyboard focus so the
            // window's own Escape (close the editor) leaves the press to the capture.
            capture.armed_frame = frame;
            world.insert_resource(capture);
            response.request_focus();
        }
        if ui.selectable_label(state.show_keyboard, "Keyboard map").clicked() {
            state.show_keyboard = !state.show_keyboard;
        }
        if let Some(chord) = state.chord_filter {
            ui.label(format!("Showing {}", chord.label()));
            if ui.small_button("Clear").clicked() {
                state.chord_filter = None;
            }
        }
    });
    if state.show_keyboard {
        keyboard_map(ui, &keymap, state);
    }
    if let Some(chord) = state.chord_filter {
        let found = lookup(&keymap, chord);
        if found.uses.is_empty() && found.fixed.is_empty() {
            ui.weak(format!("No command uses {}", found.keys));
        }
        for u in &found.uses {
            ui.label(format!(
                "{}: {} ({}{})",
                found.keys,
                u.title,
                u.scope.label(),
                if u.user { ", your shortcut" } else { ", built-in" }
            ));
        }
        for f in &found.fixed {
            ui.weak(format!("{}: {f} (fixed)", found.keys));
        }
        if found.conflict {
            ui.colored_label(crate::theme::ui_tokens(ui.ctx()).warn, "These uses collide");
        }
        ui.separator();
    }
    let query = state.query.trim().to_lowercase();
    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("prefs_keymap")
            .num_columns(5)
            .striped(true)
            .show(ui, |ui| {
                for def in COMMANDS.iter() {
                    let text = format!("{}: {} {}", def.category, def.title, def.id).to_lowercase();
                    if !query.is_empty() && !text.contains(&query) {
                        continue;
                    }
                    let tip = crate::tooltips::RichTooltip::for_command(def, &keymap);
                    crate::tooltips::attach(ui.label(format!("{}: {}", def.category, def.title)), &tip);
                    let shown = if keymap.recording == Some(def.id) {
                        "press a key (Esc clears)".to_owned()
                    } else {
                        keymap.shortcut_of(def)
                    };
                    let user = keymap.bindings.get(def.id).copied();
                    if user.is_some() {
                        ui.strong(shown);
                    } else {
                        ui.label(shown);
                    }
                    match user {
                        Some(chord) => {
                            let current = keymap.scope_of(def.id);
                            let mut picked = None;
                            let combo = egui::ComboBox::from_id_salt(("scope", def.id))
                                .selected_text(current.label())
                                .show_ui(ui, |ui| {
                                    for scope in ShortcutScope::ALL {
                                        if ui.selectable_label(scope == current, scope.label()).clicked() {
                                            picked = Some(scope);
                                        }
                                    }
                                });
                            crate::a11y_audit::name(
                                combo.response,
                                egui::WidgetType::ComboBox,
                                &format!("Scope of {}", def.title),
                            );
                            if let Some(scope) = picked.filter(|s| *s != current)
                                && let Err(err) = keymap.bind_in(def.id, chord, scope)
                            {
                                state.status = match err {
                                    crate::command_palette::BindError::SafetyKey => format!(
                                        "shortcut_safety_key: {} stays global (stop play, save, undo and redo)",
                                        chord.label()
                                    ),
                                    other => format!("{}: {other:?}", def.id),
                                };
                            }
                        }
                        None => {
                            ui.weak(builtin_scope(def.id).label());
                        }
                    }
                    if ui.small_button("Set…").clicked() {
                        keymap.recording = Some(def.id);
                    }
                    if keymap.bindings.contains_key(def.id) {
                        if ui.small_button("Default").clicked() {
                            keymap.unbind(def.id);
                        }
                    } else {
                        ui.label("");
                    }
                    ui.end_row();
                }
            });
        let found = conflicts(&keymap);
        if !found.is_empty() {
            ui.separator();
            let warn = crate::theme::ui_tokens(ui.ctx()).warn;
            for c in &found {
                ui.colored_label(warn, format!("{} on {} hides {}", c.keys, c.command, c.shadows));
            }
        }
    });
    ui.separator();
    ui.horizontal(|ui| {
        if ui.button("Import…").clicked()
            && let Some(path) = crate::file_dialog::FileDialog::new()
                .add_filter("Keymap", &["ron"])
                .pick_file()
        {
            let mode = import_mode(state.import_replace);
            match std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|text| import_keymap(&keymap, &text, mode).map_err(|e| e.to_string()))
            {
                Ok(report) => {
                    state.status = format!(
                        "imported {} bindings ({} problems, {} conflicts)",
                        report.applied.len(),
                        report.problems.len(),
                        report.conflicts.len()
                    );
                    keymap = report.keymap;
                }
                Err(err) => state.status = format!("import failed: {err}"),
            }
        }
        if ui.button("Export…").clicked()
            && let Some(path) = crate::file_dialog::FileDialog::new()
                .add_filter("Keymap", &["ron"])
                .set_file_name("editor_keymap.ron")
                .save_file()
        {
            let text = keymap_to_text(&keymap, &store.unknown);
            state.status = match crate::level_io::write_atomic(&path, text.as_bytes()) {
                Ok(()) => format!("exported to {}", path.display()),
                Err(err) => format!("export failed: {err}"),
            };
        }
        if ui.button("Reset all shortcuts").clicked() {
            keymap.bindings.clear();
            keymap.scopes.clear();
            keymap.recording = None;
            state.status = "every command is back to its built-in shortcut".to_owned();
        }
        import_replace_toggle(ui, state);
    });
    if keymap != original {
        world.insert_resource(keymap);
    }
    // Extension commands: chords, rebinding and conflicts (ADR-7791).
    crate::extension::actions::keymap_section(ui, world);
}

/// The preset picker and Save preset.
fn presets_row(ui: &mut egui::Ui, world: &mut World, state: &mut PrefsPanelState) {
    // The preset files are listed once, and again after a save (not every frame).
    if state.presets.is_none() {
        let dir = presets_dir(world.get_resource::<EditorConfigPaths>());
        state.presets = Some(list_presets(dir.as_deref()));
    }
    let presets = state.presets.clone().unwrap_or_default();
    let active = world
        .get_resource::<KeymapStore>()
        .and_then(|s| s.active_preset.clone());
    let mut apply = None;
    ui.horizontal(|ui| {
        ui.label("Preset");
        let combo = egui::ComboBox::from_id_salt("keymap_preset")
            .selected_text(active.clone().unwrap_or_else(|| "(custom)".to_owned()))
            .show_ui(ui, |ui| {
                for p in &presets {
                    if ui
                        .selectable_label(active.as_deref() == Some(p.name.as_str()), p.name.as_str())
                        .clicked()
                    {
                        apply = Some(p.name.clone());
                    }
                }
            });
        crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Keymap preset");
        let name = ui.add(
            egui::TextEdit::singleline(&mut state.preset_name)
                .hint_text("Preset name")
                .desired_width(140.0),
        );
        crate::a11y_audit::name(name, egui::WidgetType::TextEdit, "Preset name");
        if ui.button("Save preset").clicked() {
            state.status = match save_preset(world, state.preset_name.trim()) {
                Ok(path) => format!("saved {}", path.display()),
                Err(err) => format!("{}: {}", err.code, err.message),
            };
            state.presets = None;
        }
    });
    if let Some(name) = apply {
        state.status = match apply_preset(world, &name) {
            Ok(report) => format!("{name}: {} shortcuts", report.applied.len()),
            Err(err) => format!("{}: {}", err.code, err.message),
        };
    }
}

/// The drawn keyboard: each key a focusable button colored bound, unbound or conflict for the
/// chosen modifiers; activating a key lists its commands.
fn keyboard_map(ui: &mut egui::Ui, keymap: &CommandKeymap, state: &mut PrefsPanelState) {
    let _span = tracing::trace_span!(target: "regia_editor", "keyboard_map").entered();
    let tokens = crate::theme::ui_tokens(ui.ctx());
    ui.horizontal(|ui| {
        for (i, name) in ["Ctrl", "Shift", "Alt"].into_iter().enumerate() {
            if ui.selectable_label(state.map_modifiers[i], name).clicked() {
                state.map_modifiers[i] = !state.map_modifiers[i];
            }
        }
        ui.weak("Bound, unbound, conflict");
    });
    let [command, shift, alt] = state.map_modifiers;
    let states = key_states(keymap, command, shift, alt);
    let width = ui.spacing().interact_size.y * 1.6;
    let mut index = 0;
    for row in KEYBOARD_ROWS {
        ui.horizontal(|ui| {
            for &key in *row {
                let st = states.get(index).map_or(KeyState::Unbound, |(_, s)| *s);
                index += 1;
                let (fill, text) = match st {
                    KeyState::Unbound => (tokens.well, tokens.muted),
                    KeyState::Bound => (tokens.signal, tokens.on_signal),
                    KeyState::Conflict => (tokens.warn, tokens.on_signal),
                };
                let button = egui::Button::new(egui::RichText::new(key.symbol_or_name()).color(text))
                    .fill(fill)
                    .min_size(egui::vec2(width, 0.0));
                // Arrow keys draw a glyph: the key name is the accessible name.
                let response = crate::a11y_audit::name(ui.add(button), egui::WidgetType::Button, key.name());
                if response.clicked() {
                    state.chord_filter = Some(KeyChord {
                        command,
                        shift,
                        alt,
                        key,
                    });
                }
            }
        });
    }
}

fn project_tab(ui: &mut egui::Ui, world: &mut World, state: &mut PrefsPanelState) {
    other_matches(ui, state);
    let root = project_root(world);
    if root.is_none() {
        ui.weak("Open a project to search its Regia.toml.");
        return;
    }
    if let Some(err) = &state.project_error {
        ui.colored_label(crate::theme::ui_tokens(ui.ctx()).warn, err);
    }
    ui.horizontal(|ui| {
        if ui.button("Open Project Settings").clicked() {
            crate::editor_layout::LayoutRequests::push(
                world,
                crate::editor_layout::LayoutRequest::Open(crate::shell::Panel::ProjectSettings),
            );
        }
        if ui.button("Reload").clicked() {
            state.project_loaded_for = None;
        }
    });
    let hits = search_rows(&state.project_rows, &state.query);
    if hits.is_empty() {
        ui.weak("No project setting matches");
    }
    let changed_categories: std::collections::BTreeSet<&str> = state
        .project_rows
        .iter()
        .filter(|row| row.changed == Some(true))
        .filter_map(|row| row.key.rsplit_once('.').map(|(parent, _)| parent))
        .collect();
    let mut reset: Option<(Vec<String>, Option<String>)> = None;
    egui::ScrollArea::vertical().show(ui, |ui| {
        let mut category = String::new();
        egui::Grid::new("prefs_project")
            .num_columns(3)
            .striped(true)
            .show(ui, |ui| {
                for row in hits {
                    let parent = row.key.rsplit_once('.').map_or("", |(parent, _)| parent);
                    if category != parent {
                        category = parent.to_owned();
                        ui.strong(&category);
                        ui.label("");
                        let any_changed = changed_categories.contains(parent);
                        if any_changed && ui.small_button(format!("Reset {parent}")).clicked() {
                            reset = Some((Vec::new(), Some(parent.to_owned())));
                        }
                        ui.end_row();
                    }
                    let label = if row.changed == Some(true) {
                        egui::RichText::new(format!("● {}", row.key)).strong()
                    } else {
                        egui::RichText::new(&row.key)
                    };
                    ui.label(label).on_hover_text(match &row.default {
                        Some(default) => format!("Default: {default}"),
                        None => "No documented default; this key is preserved by Reset.".into(),
                    });
                    ui.monospace(&row.value);
                    if row.changed == Some(true) && ui.small_button("Revert").clicked() {
                        reset = Some((vec![row.key.clone()], None));
                    }
                    ui.end_row();
                }
            });
    });
    if let Some((keys, category)) = reset {
        let id = egui::Id::new("project_settings_reset_frame");
        let frame = ui.ctx().cumulative_frame_nr();
        let already_applied = ui.data_mut(|data| data.get_temp::<u64>(id)) == Some(frame);
        if !already_applied {
            ui.data_mut(|data| data.insert_temp(id, frame));
            match crate::project_settings_ext::revert_project_settings(world, &keys, category.as_deref()) {
                Ok(_) => {
                    state.status = "Project settings reverted (Undo restores the previous values).".into();
                    state.project_loaded_for = None;
                    state.refresh_project(root);
                }
                Err(error) => state.status = error.message,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn raw(events: Vec<egui::Event>, frame: u32) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 1000.0),
            )),
            time: Some(f64::from(frame) / 60.0),
            predicted_dt: 1.0 / 60.0,
            events,
            ..Default::default()
        }
    }

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// One editor frame: the palette's key handler (which captures chords) then the panel.
    fn frame(ctx: &egui::Context, world: &mut World, events: Vec<egui::Event>, n: &mut u32) {
        *n += 1;
        let mut out = ctx.run_ui(raw(events, *n), |ui| {
            crate::command_palette::show(ui.ctx(), world, false);
            egui::CentralPanel::default().show(ui, |ui| preferences_panel(ui, world));
        });
        out.textures_delta.clear();
    }

    /// From a fresh Keymap tab, presses Tab `tabs` times then Enter; answers the world.
    fn tab_then_enter(tabs: usize, setup: &dyn Fn(&mut World)) -> World {
        let ctx = egui::Context::default();
        let mut world = World::new();
        setup(&mut world);
        let mut n = 0;
        frame(&ctx, &mut world, vec![], &mut n);
        for _ in 0..tabs {
            frame(&ctx, &mut world, vec![key(egui::Key::Tab)], &mut n);
        }
        frame(&ctx, &mut world, vec![key(egui::Key::Enter)], &mut n);
        frame(&ctx, &mut world, vec![], &mut n);
        world
    }

    fn keymap_tab_state(world: &mut World) {
        world.insert_resource(PrefsPanelState {
            keymap_tab: true,
            ..Default::default()
        });
    }

    #[test]
    fn press_a_key_is_reachable_by_keyboard_and_lists_the_chord() {
        // Tab through the panel until Enter on the focused widget starts the capture.
        let reached = (1..=12).find(|&tabs| {
            let world = tab_then_enter(tabs, &keymap_tab_state);
            world.get_resource::<ChordCapture>().is_some_and(|c| c.active)
        });
        let tabs = reached.expect("Press a key… is reachable with Tab and Enter");
        // Then the next key press is captured (no shortcut runs) and becomes the filter.
        let ctx = egui::Context::default();
        let mut world = World::new();
        keymap_tab_state(&mut world);
        let mut n = 0;
        frame(&ctx, &mut world, vec![], &mut n);
        for _ in 0..tabs {
            frame(&ctx, &mut world, vec![key(egui::Key::Tab)], &mut n);
        }
        frame(&ctx, &mut world, vec![key(egui::Key::Enter)], &mut n);
        frame(&ctx, &mut world, vec![key(egui::Key::W)], &mut n);
        frame(&ctx, &mut world, vec![], &mut n);
        let state = world.resource::<PrefsPanelState>();
        assert_eq!(state.chord_filter, KeyChord::parse("W"));
        assert!(!world.resource::<ChordCapture>().active);
        let found = lookup(world.resource::<CommandKeymap>(), KeyChord::parse("W").unwrap());
        assert!(found.uses.iter().any(|u| u.id == "gizmo.translate"), "{found:?}");
    }

    #[test]
    fn the_keyboard_map_opens_from_the_keyboard() {
        let reached = (1..=12).any(|tabs| {
            let world = tab_then_enter(tabs, &keymap_tab_state);
            world.resource::<PrefsPanelState>().show_keyboard
        });
        assert!(reached, "Keyboard map is reachable with Tab and Enter");
    }

    #[test]
    fn reset_category_is_reachable_by_keyboard() {
        let setup = |world: &mut World| {
            let mut prefs = EditorPreferences::default();
            prefs.set("camera.fly_speed", &PrefValue::Float(20.0)).unwrap();
            world.insert_resource(prefs);
            world.insert_resource(PrefsPanelState {
                query: "fly speed".into(),
                ..Default::default()
            });
        };
        let reached = (1..=12).any(|tabs| {
            let world = tab_then_enter(tabs, &setup);
            world
                .resource::<EditorPreferences>()
                .is_default("camera.fly_speed")
        });
        assert!(
            reached,
            "Reset category (or Reset) is reachable with Tab and Enter"
        );
    }

    #[test]
    fn project_revert_and_category_reset_are_keyboard_operable() {
        let root = regia_test_utils::unique_temp_dir("project-reset-keyboard");
        std::fs::create_dir_all(&root).unwrap();
        let original = "[project]\nname='Demo'\nid='com.example.demo'\nversion='2.0.0'\nbuild_number=42\n";
        let mut reached_key = false;
        let mut reached_category = false;
        for tabs in 1..=18 {
            let setup = |world: &mut World| {
                crate::level_io::write_atomic(&root.join("Regia.toml"), original.as_bytes()).unwrap();
                world.insert_resource(crate::agent_ext::EditorProject {
                    root: Some(root.clone()),
                });
                world.insert_resource(crate::command::UndoStack::default());
                world.insert_resource(PrefsPanelState {
                    project_tab: true,
                    query: "project.version".into(),
                    ..Default::default()
                });
            };
            let world = tab_then_enter(tabs, &setup);
            let table: toml::Table =
                toml::from_str(&std::fs::read_to_string(root.join("Regia.toml")).unwrap()).unwrap();
            if table["project"]["version"].as_str() == Some("0.1.0") {
                assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), 1);
                if table["project"]["build_number"].as_integer() == Some(1) {
                    reached_category = true;
                } else {
                    reached_key = true;
                }
            }
        }
        assert!(reached_key, "Revert is reached with Tab and Enter");
        assert!(
            reached_category,
            "Reset project includes the filtered-out build_number"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_shared_search_finds_project_settings_keys() {
        let root = regia_test_utils::unique_temp_dir("prefs-project-search");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("Regia.toml"),
            "[project]\nname = 'Demo'\n[platforms.android]\nrender_profile = 'mobile'\n",
        )
        .unwrap();
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        world.insert_resource(PrefsPanelState {
            query: "render_profile".into(),
            ..Default::default()
        });
        let ctx = egui::Context::default();
        let mut n = 0;
        frame(&ctx, &mut world, vec![], &mut n);
        let state = world.resource::<PrefsPanelState>();
        let hits = search_rows(&state.project_rows, &state.query);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, "platforms.android.render_profile");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn every_keymap_widget_is_named_and_a_map_key_is_reachable_by_tab() {
        use crate::canvas_keys::test_input::{frame as ak_frame, key as ak_key, tab_to};
        let mut world = World::new();
        world.insert_resource(PrefsPanelState {
            keymap_tab: true,
            show_keyboard: true,
            query: "zzzz".into(), // no grid rows: the map's keys come early in the Tab order
            ..Default::default()
        });
        let ctx = egui::Context::default();
        let size = [1400.0, 1000.0];
        let mut draw = |ui: &mut egui::Ui| {
            egui::CentralPanel::default().show(ui, |ui| preferences_panel(ui, &mut world));
        };
        ak_frame(&ctx, size, vec![], &mut draw);
        let update = ak_frame(&ctx, size, vec![], &mut draw);
        assert_eq!(crate::a11y_audit::unnamed_widgets(&update), Vec::<String>::new());
        tab_to(&ctx, size, "Down", false, 120, &mut draw);
        ak_frame(
            &ctx,
            size,
            vec![ak_key(egui::Key::Enter, egui::Modifiers::NONE)],
            &mut draw,
        );
        drop(draw);
        assert_eq!(
            world.resource::<PrefsPanelState>().chord_filter,
            KeyChord::parse("ArrowDown")
        );
        // The Preferences tab (every row, one changed) and the Project Settings tab name every widget too.
        let mut prefs = EditorPreferences::default();
        prefs.set("camera.fly_speed", &PrefValue::Float(20.0)).unwrap();
        world.insert_resource(prefs);
        for project_tab in [false, true] {
            world.insert_resource(PrefsPanelState {
                project_tab,
                ..Default::default()
            });
            let ctx = egui::Context::default();
            let mut draw = |ui: &mut egui::Ui| {
                egui::CentralPanel::default().show(ui, |ui| preferences_panel(ui, &mut world));
            };
            ak_frame(&ctx, size, vec![], &mut draw);
            let update = ak_frame(&ctx, size, vec![], &mut draw);
            assert_eq!(
                crate::a11y_audit::unnamed_widgets(&update),
                Vec::<String>::new(),
                "project tab {project_tab}"
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Theme (spec addendum § 21, ADR-7838)

/// The Theme page's own state: the name typed for Save as.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct ThemePageState {
    /// The name the next Save as uses.
    pub save_name: String,
}

/// Accessible name of the accent color button on the Theme page (and its hex field with ` hex`).
pub const ACCENT_NAME: &str = "Accent color";

/// A color row: the label, a color button and a hex field, both named for screen readers. The hex
/// field keeps what is typed until it parses (`#rrggbb` or `#rrggbbaa`), so it is the keyboard path
/// to every color. Returns the new color when either changed it.
fn color_row(ui: &mut egui::Ui, id: &str, name: &str, color: egui::Color32) -> Option<egui::Color32> {
    let mut next = None;
    ui.horizontal(|ui| {
        ui.label(name);
        let mut picked = color;
        let button = egui::color_picker::color_edit_button_srgba(
            ui,
            &mut picked,
            egui::color_picker::Alpha::OnlyBlend,
        );
        let button = crate::a11y_audit::name(button, egui::WidgetType::ColorButton, name);
        if button.changed() && picked != color {
            next = Some(picked);
        }
        let hex_id = ui.make_persistent_id(("theme_hex", id));
        let current = hex_text(next.unwrap_or(color));
        let mut text: String = ui
            .data_mut(|d| d.get_temp(hex_id))
            .unwrap_or_else(|| current.clone());
        let field = ui.add(egui::TextEdit::singleline(&mut text).desired_width(90.0));
        let field = crate::a11y_audit::name(field, egui::WidgetType::TextEdit, &format!("{name} hex"));
        if field.changed()
            && let Ok(parsed) = crate::theme::parse_color(&serde_json::Value::String(text.trim().to_string()))
            && parsed != color
        {
            next = Some(parsed);
        }
        if field.has_focus() {
            ui.data_mut(|d| d.insert_temp(hex_id, text));
        } else {
            ui.data_mut(|d| d.remove::<String>(hex_id));
        }
    });
    next
}

fn hex_text(color: egui::Color32) -> String {
    let [r, g, b, a] = color.to_srgba_unmultiplied();
    if a == 255 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
    }
}

/// The Theme tab: the theme choice and saved themes, the accent and every token with a live
/// preview, the diagnostic colors, Save as, import and export. Token edits change the world's
/// theme at once (`editor.set_theme`); Save as keeps them (`editor.theme_save`).
#[tracing::instrument(target = "regia_editor", skip_all, name = "preferences_theme_tab")]
fn theme_tab(ui: &mut egui::Ui, world: &mut World, state: &mut PrefsPanelState) {
    use crate::editor_prefs::{AppearanceStatus, CUSTOM_THEME_PREF, DIAGNOSTIC_COLOR_PREFS, THEME_CHOICES};
    let mut page = world.remove_resource::<ThemePageState>().unwrap_or_default();
    let mut prefs = world
        .get_resource_or_insert_with(EditorPreferences::default)
        .clone();
    let before = prefs.generation();
    let config_dir = world
        .get_resource::<crate::editor_prefs::EditorConfigPaths>()
        .and_then(|p| p.dir.clone());
    let saved = config_dir
        .as_deref()
        .map(crate::theme::list_custom_themes)
        .unwrap_or_default();
    let mut tokens = crate::theme::tokens(world).clone();
    let original = tokens.clone();
    let warn = crate::theme::ui_tokens(ui.ctx()).warn;
    if let Some(err) = world
        .get_resource::<AppearanceStatus>()
        .and_then(|s| s.custom_theme_error.clone())
    {
        ui.colored_label(warn, format!("Custom theme not loaded: {err}"));
    }
    let custom = prefs.text(CUSTOM_THEME_PREF);
    let choice = prefs.text("appearance.theme");
    ui.horizontal(|ui| {
        ui.label("Theme");
        let shown = if custom.is_empty() {
            crate::theme::theme_display_name(&choice)
        } else {
            format!("{custom} (saved)")
        };
        let combo = egui::ComboBox::from_id_salt("prefs_theme_choice")
            .selected_text(shown)
            .show_ui(ui, |ui| {
                for c in THEME_CHOICES {
                    if ui
                        .selectable_label(
                            custom.is_empty() && choice == *c,
                            crate::theme::theme_display_name(c),
                        )
                        .clicked()
                    {
                        let _ = prefs.set("appearance.theme", &PrefValue::Text((*c).to_owned()));
                        let _ = prefs.reset(CUSTOM_THEME_PREF);
                    }
                }
                for name in &saved {
                    if ui
                        .selectable_label(custom == *name, format!("{name} (saved)"))
                        .clicked()
                    {
                        let _ = prefs.set(CUSTOM_THEME_PREF, &PrefValue::Text(name.clone()));
                    }
                }
            });
        crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Theme");
    });
    if let Some(color) = color_row(ui, "signal", ACCENT_NAME, tokens.signal) {
        tokens.signal = color;
    }
    let failing = crate::a11y_audit::editor_contrast(&tokens)
        .iter()
        .filter(|row| !row.passes())
        .count();
    if failing == 0 {
        ui.weak("Contrast: every text and control pair meets WCAG AA");
    } else {
        ui.colored_label(
            warn,
            format!("Contrast: {failing} text or control pairs are below WCAG AA."),
        )
        .on_hover_text("Agent: editor.a11y_contrast lists them");
    }
    ui.collapsing("Diagnostic colors", |ui| {
        let defaults = {
            let mut base = tokens.clone();
            crate::editor_prefs::diagnostic_colors(&prefs).apply_to(&mut base);
            base
        };
        let rows = [
            (
                DIAGNOSTIC_COLOR_PREFS[0],
                "Error color",
                crate::theme::DiagnosticLevel::Error,
            ),
            (
                DIAGNOSTIC_COLOR_PREFS[1],
                "Warning color",
                crate::theme::DiagnosticLevel::Warning,
            ),
            (
                DIAGNOSTIC_COLOR_PREFS[2],
                "Info color",
                crate::theme::DiagnosticLevel::Info,
            ),
            (
                DIAGNOSTIC_COLOR_PREFS[3],
                "Success color",
                crate::theme::DiagnosticLevel::Success,
            ),
        ];
        for (key, label, level) in rows {
            let shown = crate::theme::diagnostic_color(&defaults, level);
            if let Some(color) = color_row(ui, key, label, shown) {
                let [r, g, b, _] = color.to_srgba_unmultiplied();
                if let Err(err) = prefs.set(key, &PrefValue::Text(format!("#{r:02x}{g:02x}{b:02x}"))) {
                    state.status = err.to_string();
                }
            }
            if !prefs.is_default(key)
                && crate::a11y_audit::glyph_button(ui, "Use theme", &format!("{label}: use the theme's"))
                    .clicked()
            {
                let _ = prefs.reset(key);
            }
        }
    });
    ui.collapsing("Every color", |ui| {
        egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
            for (key, slot) in tokens.color_slots_mut() {
                if let Some(color) = color_row(ui, &key, &crate::theme::token_label(&key), *slot) {
                    *slot = color;
                }
            }
        });
    });
    ui.separator();
    ui.horizontal(|ui| {
        ui.label("Name");
        let field = ui.add(egui::TextEdit::singleline(&mut page.save_name).desired_width(160.0));
        crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "Theme name");
        if ui.button("Save theme as…").clicked() {
            state.status = match &config_dir {
                None => "Not saved: no user config directory this session".to_owned(),
                Some(dir) => match crate::theme::save_custom_theme(dir, &page.save_name, &tokens) {
                    Ok(path) => {
                        let _ = prefs.set(CUSTOM_THEME_PREF, &PrefValue::Text(page.save_name.clone()));
                        format!("Saved {}", path.display())
                    }
                    Err(err) => format!("Not saved: {err}"),
                },
            };
        }
    });
    ui.horizontal(|ui| {
        if ui.button("Import theme…").clicked()
            && let Some(path) = crate::file_dialog::FileDialog::new()
                .add_filter("Editor theme", &["ron"])
                .pick_file()
        {
            state.status = match (crate::theme::read_theme_file(&path), &config_dir) {
                (Err(err), _) => format!("Import failed: {err}"),
                (Ok(_), None) => "Import failed: no user config directory this session".to_owned(),
                (Ok((name, imported)), Some(dir)) => {
                    let name = if crate::theme::valid_theme_name(&name) {
                        name
                    } else {
                        "imported".to_owned()
                    };
                    match crate::theme::save_custom_theme(dir, &name, &imported) {
                        Ok(_) => {
                            let _ = prefs.set(CUSTOM_THEME_PREF, &PrefValue::Text(name.clone()));
                            format!("Imported {name}")
                        }
                        Err(err) => format!("Import failed: {err}"),
                    }
                }
            };
        }
        if ui.button("Export theme…").clicked()
            && let Some(path) = crate::file_dialog::FileDialog::new()
                .add_filter("Editor theme", &["ron"])
                .set_file_name(format!("my{}", crate::theme::THEME_FILE_SUFFIX))
                .save_file()
        {
            let name = if page.save_name.is_empty() {
                "exported"
            } else {
                page.save_name.as_str()
            };
            let text = crate::theme::theme_file_to_text(name, &tokens);
            state.status = match crate::level_io::write_atomic(&path, text.as_bytes()) {
                Ok(()) => format!("Exported to {}", path.display()),
                Err(err) => format!("Export failed: {err}"),
            };
        }
        if ui.button("Revert edits").clicked() {
            crate::editor_prefs::apply_appearance(world);
            tokens = crate::theme::tokens(world).clone();
            state.status = "Theme edits reverted".to_owned();
        }
    });
    if tokens != original {
        world.insert_resource(tokens);
    }
    if prefs.generation() != before {
        world.insert_resource(prefs);
    }
    world.insert_resource(page);
}

#[cfg(test)]
mod theme_tab_tests {
    use super::*;

    fn frame(ctx: &egui::Context, world: &mut World, events: Vec<egui::Event>) -> egui::FullOutput {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 700.0),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default().show(ui, |ui| preferences_panel(ui, world));
        });
        output.textures_delta.clear();
        output
    }

    fn theme_world() -> World {
        let mut world = World::new();
        world.insert_resource(PrefsPanelState {
            theme_tab: true,
            ..Default::default()
        });
        world
    }

    fn focused_name(output: &egui::FullOutput) -> Option<String> {
        let update = output.platform_output.accesskit_update.as_ref()?;
        update
            .nodes
            .iter()
            .find(|(id, _)| *id == update.focus)
            .and_then(|(_, node)| node.label().map(str::to_string))
    }

    fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    /// The keyboard path: Tab reaches the accent's hex field, typing a color there changes the live
    /// theme (no pointer event at all).
    #[test]
    fn the_accent_is_reachable_and_editable_from_the_keyboard() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut world = theme_world();
        frame(&ctx, &mut world, Vec::new());
        let target = format!("{ACCENT_NAME} hex");
        let mut reached = false;
        let mut seen = Vec::new();
        for _ in 0..40 {
            let output = frame(&ctx, &mut world, vec![key(egui::Key::Tab, egui::Modifiers::NONE)]);
            let name = focused_name(&output).unwrap_or_default();
            if name == target {
                reached = true;
                break;
            }
            seen.push(name);
        }
        assert!(reached, "Tab never reached {target}: {seen:?}");
        let mut events = vec![key(egui::Key::End, egui::Modifiers::NONE)];
        events.extend((0..12).map(|_| key(egui::Key::Backspace, egui::Modifiers::NONE)));
        events.push(egui::Event::Text("#00ff00".to_string()));
        frame(&ctx, &mut world, events);
        assert_eq!(
            crate::theme::tokens(&world).signal,
            egui::Color32::from_rgb(0, 255, 0),
            "typing the hex changed the accent"
        );
    }

    /// The named-widget rule (a11y_audit): every interactive node of the Theme tab has a name.
    #[test]
    fn every_theme_tab_widget_is_named() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut world = theme_world();
        frame(&ctx, &mut world, Vec::new());
        let output = frame(&ctx, &mut world, Vec::new());
        let update = output.platform_output.accesskit_update.expect("accesskit is on");
        let names = crate::a11y_audit::widget_names(&update);
        assert!(
            crate::a11y_audit::unnamed_widgets(&update).is_empty(),
            "{names:?}"
        );
        for want in ["Theme", "Theme name", "Save theme as…", "Search preferences"] {
            assert!(names.iter().any(|(_, n)| n == want), "{want} missing: {names:?}");
        }
        // A color button is an AccessKit color well (outside the audit's operable roles): its name
        // is checked on the node itself.
        assert!(
            update
                .nodes
                .iter()
                .any(|(_, node)| node.label() == Some(ACCENT_NAME)),
            "the accent's color button is named"
        );
    }
}
