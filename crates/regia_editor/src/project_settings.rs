// SPDX-License-Identifier: MPL-2.0
//! The Project Settings panel: `Regia.toml` editing that preserves unknown keys
//! (`docs/specs/19-editor.md` § 2 "Project Settings").
//!
//! The manifest is parsed as a generic TOML table, so keys this panel does not model (render
//! profiles, packaging options, Steam config) survive a save untouched. The modelled fields are
//! the general ones the editor itself consumes: project name, id, version, build number, the
//! startup level, the `[cook.chunks.*]` rules and `[content] base_url` (spec 40 § 6, ADR-1876).
//! `config/*.ron` files are listed. The input maps in `config/input.ron` are edited in this
//! panel and by `editor.project_input_edit`: each edit rewrites that file at once in canonical
//! RON (comments are not kept; the file is created when it is missing).

use std::path::Path;

use regia_asset_pipeline::chunks::{ChunkRules, ContentMode};

/// The panel's state: the parsed manifest and where it came from.
#[derive(Debug, Default)]
pub struct ProjectSettingsState {
    /// The loaded manifest table (`None` = no project / not yet loaded).
    table: Option<toml::Value>,
    /// Whether unsaved edits exist.
    dirty: bool,
    /// The status line for the last action.
    pub toast: String,
    /// The XR section (`config/xr.ron`).
    pub xr: crate::xr_settings::XrSettings,
    /// The "Add chunk" text box.
    new_chunk: String,
    /// Undo entries of `config/input.ron` writes the panel made; the shell moves them to the
    /// editor's undo stack ([`crate::project_settings_ext::push_input_undo`]).
    pub(crate) input_undo: Vec<crate::project_settings_ext::InputConfigEdit>,
    /// Undo entries of `config/physics.ron` writes the layer matrix made (ADR-7233); the shell
    /// pushes them like `input_undo`.
    pub(crate) physics_undo: Vec<crate::physics_matrix::PhysicsMatrixEdit>,
    /// Undo entries of `Regia.toml` saves the content-chunk settings made (CT-26); the shell
    /// pushes them like `input_undo`.
    pub(crate) manifest_undo: Vec<crate::closure_settings::ManifestEdit>,
    /// The content-chunk settings preview (computed off-thread, CT-26).
    pub(crate) closure_preview: crate::closure_settings::PreviewWorker,
    /// [`crate::closure_settings::manifest_revision`] at the last load: an undo that rewrote
    /// `Regia.toml` reloads an unedited panel.
    loaded_revision: u64,
}

impl ProjectSettingsState {
    /// Loads `<project>/Regia.toml` (unknown keys included).
    ///
    /// # Errors
    ///
    /// When the file cannot be read or parsed.
    pub fn load(&mut self, project_root: &Path) -> Result<(), String> {
        let path = manifest_path(project_root);
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let table: toml::Value =
            toml::from_str(&text).map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
        self.table = Some(table);
        self.dirty = false;
        self.loaded_revision = crate::closure_settings::manifest_revision();
        self.toast = format!("loaded {}", path.display());
        Ok(())
    }

    /// The loaded manifest's root table (`None` before a load).
    #[must_use]
    pub fn manifest(&self) -> Option<&toml::Table> {
        self.table.as_ref().and_then(toml::Value::as_table)
    }

    /// The loaded manifest's root table for editing (`None` before a load); the caller marks the
    /// state dirty ([`Self::mark_dirty`]).
    pub fn manifest_mut(&mut self) -> Option<&mut toml::Table> {
        let table = self.table.as_mut()?;
        if !table.is_table() {
            *table = toml::Value::Table(toml::Table::new());
        }
        table.as_table_mut()
    }

    /// Records an edit made through [`Self::manifest_mut`].
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Whether a manifest is loaded.
    #[must_use]
    pub fn is_loaded(&self) -> bool {
        self.table.is_some()
    }

    /// Whether the panel holds edits `Regia.toml` does not have yet.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Reads a `[project]` string field.
    #[must_use]
    pub fn project_string(&self, key: &str) -> String {
        self.table
            .as_ref()
            .and_then(|table| table.get("project"))
            .and_then(|project| project.get(key))
            .and_then(toml::Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    /// Reads a `[project]` integer field.
    #[must_use]
    pub fn project_int(&self, key: &str) -> Option<i64> {
        self.table
            .as_ref()
            .and_then(|table| table.get("project"))
            .and_then(|project| project.get(key))
            .and_then(toml::Value::as_integer)
    }

    /// Writes a `[project]` string field (creating the table when absent).
    pub fn set_project_string(&mut self, key: &str, value: &str) {
        self.with_project_table(|project, table| {
            table.insert(key.to_string(), toml::Value::String(value.to_string()));
            let _ = project;
        });
        self.dirty = true;
    }

    /// Writes a `[project]` integer field (creating the table when absent).
    pub fn set_project_int(&mut self, key: &str, value: i64) {
        self.with_project_table(|_, table| {
            table.insert(key.to_string(), toml::Value::Integer(value));
        });
        self.dirty = true;
    }

    /// Runs `body` against the `[project]` table, inserting it when absent.
    fn with_project_table(&mut self, body: impl FnOnce(Option<&toml::Value>, &mut toml::Table)) {
        let Some(table) = self.table.as_mut() else {
            return;
        };
        if !table.is_table() {
            *table = toml::Value::Table(toml::Table::new());
        }
        let root = table.as_table_mut().expect("made a table above");
        let project = root
            .entry("project".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !project.is_table() {
            *project = toml::Value::Table(toml::Table::new());
        }
        let project_snapshot = project.clone();
        let project_table = project.as_table_mut().expect("made a table above");
        body(Some(&project_snapshot), project_table);
    }

    /// The `[cook.chunks.*]` rules, sorted by name. The old `name = ["globs"]` shorthand reads as a
    /// rule with `include` only.
    #[must_use]
    pub fn chunk_rules(&self) -> Vec<ChunkRuleEdit> {
        let Some(chunks) = self.chunks_table() else {
            return Vec::new();
        };
        chunks
            .iter()
            .map(|(name, value)| {
                let mut edit = ChunkRuleEdit {
                    name: name.clone(),
                    ..ChunkRuleEdit::default()
                };
                match value {
                    toml::Value::Array(globs) => edit.include = strings(Some(globs)),
                    toml::Value::Table(rule) => {
                        let list = |key: &str| strings(rule.get(key).and_then(toml::Value::as_array));
                        edit.include = list("include");
                        edit.entry = list("entry");
                        edit.tags = list("tags");
                        edit.requires = list("requires");
                        if let Some(delivery) = rule.get("delivery").and_then(toml::Value::as_str) {
                            delivery.clone_into(&mut edit.delivery);
                        }
                        edit.priority = rule.get("priority").and_then(toml::Value::as_integer);
                    }
                    _ => {}
                }
                edit
            })
            .collect()
    }

    fn chunks_table(&self) -> Option<&toml::Table> {
        self.table.as_ref()?.get("cook")?.get("chunks")?.as_table()
    }

    /// The `[cook.chunks]` table, created when absent; `None` before a load, or when a key on the
    /// way is not a table.
    fn chunks_table_mut(&mut self) -> Option<&mut toml::Table> {
        let cook = self
            .table
            .as_mut()?
            .as_table_mut()?
            .entry("cook")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        cook.as_table_mut()?
            .entry("chunks")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
    }

    /// Writes a chunk rule. Keys the panel does not model (`pin`, `follow`, `overrides`, unknown
    /// keys) stay; an empty list or `delivery`, or no `priority`, removes that key.
    pub fn set_chunk_rule(&mut self, edit: &ChunkRuleEdit) {
        let Some(chunks) = self.chunks_table_mut() else {
            return;
        };
        let slot = chunks
            .entry(edit.name.clone())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !slot.is_table() {
            *slot = toml::Value::Table(toml::Table::new());
        }
        let Some(rule) = slot.as_table_mut() else {
            return;
        };
        for (key, list) in [
            ("include", &edit.include),
            ("entry", &edit.entry),
            ("tags", &edit.tags),
            ("requires", &edit.requires),
        ] {
            if list.is_empty() {
                rule.remove(key);
            } else {
                let items = list.iter().cloned().map(toml::Value::String).collect();
                rule.insert(key.to_owned(), toml::Value::Array(items));
            }
        }
        if edit.delivery.is_empty() {
            rule.remove("delivery");
        } else {
            rule.insert("delivery".to_owned(), toml::Value::String(edit.delivery.clone()));
        }
        match edit.priority {
            Some(priority) => {
                rule.insert("priority".to_owned(), toml::Value::Integer(priority));
            }
            None => {
                rule.remove("priority");
            }
        }
        self.dirty = true;
    }

    /// Removes a chunk rule; returns whether it existed.
    pub fn remove_chunk_rule(&mut self, name: &str) -> bool {
        let removed = self
            .chunks_table_mut()
            .is_some_and(|chunks| chunks.remove(name).is_some());
        self.dirty |= removed;
        removed
    }

    /// `[cook.chunk_options] content` (ADR-8092): `All` when absent or not a known spelling (save
    /// refuses an unknown spelling, as the cook does).
    #[must_use]
    pub fn content_mode(&self) -> ContentMode {
        self.table
            .as_ref()
            .and_then(|t| t.get("cook")?.get("chunk_options")?.get("content")?.as_str())
            .and_then(ContentMode::parse)
            .unwrap_or_default()
    }

    /// Writes `[cook.chunk_options] content`. `All` (the default) removes the key, and the
    /// `[cook.chunk_options]` table when nothing else is in it; other keys (`shared`, unknown
    /// ones) stay.
    pub fn set_content_mode(&mut self, mode: ContentMode) {
        let Some(root) = self.table.as_mut().and_then(toml::Value::as_table_mut) else {
            return;
        };
        if mode == ContentMode::All && !root.contains_key("cook") {
            // Already the default; do not add an empty `[cook]` table.
            self.dirty = true;
            return;
        }
        let cook = root
            .entry("cook")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let Some(cook) = cook.as_table_mut() else {
            return;
        };
        match mode {
            ContentMode::All => {
                let empty = cook
                    .get_mut("chunk_options")
                    .and_then(toml::Value::as_table_mut)
                    .is_some_and(|options| {
                        options.remove("content");
                        options.is_empty()
                    });
                if empty {
                    cook.remove("chunk_options");
                }
            }
            ContentMode::Closure => {
                let options = cook
                    .entry("chunk_options")
                    .or_insert_with(|| toml::Value::Table(toml::Table::new()));
                if !options.is_table() {
                    *options = toml::Value::Table(toml::Table::new());
                }
                if let Some(options) = options.as_table_mut() {
                    options.insert("content".to_owned(), toml::Value::String(mode.name().to_owned()));
                }
            }
        }
        self.dirty = true;
    }

    /// The manifest as [`Self::save`] would write it (`None` before a load or when it does not
    /// serialize). Previews compute from this text, so they see unsaved edits.
    #[must_use]
    pub fn manifest_text(&self) -> Option<String> {
        toml::to_string_pretty(self.table.as_ref()?).ok()
    }

    /// Reads `[content] base_url` (empty when absent).
    #[must_use]
    pub fn content_base_url(&self) -> String {
        self.table
            .as_ref()
            .and_then(|t| t.get("content")?.get("base_url")?.as_str())
            .unwrap_or_default()
            .to_owned()
    }

    /// Writes `[content] base_url`; an empty text removes the key.
    pub fn set_content_base_url(&mut self, url: &str) {
        let Some(root) = self.table.as_mut().and_then(toml::Value::as_table_mut) else {
            return;
        };
        let content = root
            .entry("content")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let Some(content) = content.as_table_mut() else {
            return;
        };
        if url.is_empty() {
            content.remove("base_url");
        } else {
            content.insert("base_url".to_owned(), toml::Value::String(url.to_owned()));
        }
        self.dirty = true;
    }

    /// Saves the manifest back to `<project>/Regia.toml` (atomic write; unknown keys intact).
    ///
    /// # Errors
    ///
    /// Serialization or IO failure, or a modelled field fails validation.
    pub fn save(&mut self, project_root: &Path) -> Result<(), String> {
        let table = self.table.as_mut().ok_or("no Regia.toml is loaded")?;
        crate::debug_colors::preserve_manifest_colors(table, project_root)?;
        // Modelled-field validation before anything hits the disk.
        let name = table
            .get("project")
            .and_then(|p| p.get("name"))
            .and_then(toml::Value::as_str)
            .unwrap_or_default();
        if name.trim().is_empty() {
            return Err("[project] name must not be empty".to_string());
        }
        let id = table
            .get("project")
            .and_then(|p| p.get("id"))
            .and_then(toml::Value::as_str)
            .unwrap_or_default();
        validate_app_id(id)?;
        if let Some(root) = table.as_table() {
            ChunkRules::from_table(root).map_err(|e| format!("chunk rules: {e}"))?;
            // Platform targets, quality presets and the default level (ADR-2321).
            let blocking: Vec<String> = crate::project_settings_ext::problems(root)
                .into_iter()
                .filter(|p| p.blocks_save)
                .map(|p| format!("{}: {}", p.key, p.message))
                .collect();
            if !blocking.is_empty() {
                return Err(blocking.join("; "));
            }
            crate::hygiene_policy::validate_saved_table(root)?;
        }
        let text = toml::to_string_pretty(table).map_err(|e| format!("cannot serialize: {e}"))?;
        let path = manifest_path(project_root);
        let tmp = path.with_extension("toml.new");
        std::fs::write(&tmp, text.as_bytes()).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("cannot rename to {}: {e}", path.display()))?;
        self.dirty = false;
        self.toast = format!("saved {}", path.display());
        Ok(())
    }
}

/// One `[cook.chunks.<name>]` rule as Project Settings and `editor.content_chunks_set_rule` edit it
/// (`docs/specs/40-content-chunks-dlc-asset-packs.md` § 2.2, § 6).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChunkRuleEdit {
    /// Chunk name.
    pub name: String,
    /// `include` globs under `assets/`.
    pub include: Vec<String>,
    /// `entry` globs under `assets/`: the dynamic roots of the shipped closure with
    /// `[cook.chunk_options] content = "closure"` (content the game loads by name).
    pub entry: Vec<String>,
    /// Registry tags (`key` or `key=value`).
    pub tags: Vec<String>,
    /// Chunks this one may reference.
    pub requires: Vec<String>,
    /// `install`, `fast_follow`, `on_demand`, `external`, or empty for the default.
    pub delivery: String,
    /// 1 to 1000, `None` for the default.
    pub priority: Option<i64>,
}

fn strings(values: Option<&Vec<toml::Value>>) -> Vec<String> {
    values
        .map(|v| {
            v.iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// A comma-separated text box's items.
fn split_list(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The manifest the settings panel reads and saves: the current name, or the legacy one when only
/// that exists (ADR-1388), so a save never leaves two manifests behind.
pub(crate) fn manifest_path(project_root: &Path) -> std::path::PathBuf {
    regia_core::brand::project_manifest(project_root)
        .unwrap_or_else(|| project_root.join(regia_core::brand::MANIFEST_FILE))
}

/// Validates a reverse-DNS app id the way the CLI's manifest does (`regia_cli::manifest`:
/// two or more dot-separated segments, each starting with a letter, ASCII letters/digits/`_`,
/// and not a Java keyword).
///
/// # Errors
///
/// A message naming the offending segment.
pub fn validate_app_id(id: &str) -> Result<(), String> {
    let segments: Vec<&str> = id.split('.').collect();
    if segments.len() < 2 {
        return Err(format!(
            "[project] id `{id}` must have at least two segments, e.g. com.example.mygame"
        ));
    }
    const JAVA_RESERVED: &[&str] = &[
        "abstract",
        "assert",
        "boolean",
        "break",
        "byte",
        "case",
        "catch",
        "char",
        "class",
        "const",
        "continue",
        "default",
        "do",
        "double",
        "else",
        "enum",
        "extends",
        "false",
        "final",
        "finally",
        "float",
        "for",
        "goto",
        "if",
        "implements",
        "import",
        "instanceof",
        "int",
        "interface",
        "long",
        "native",
        "new",
        "null",
        "package",
        "private",
        "protected",
        "public",
        "return",
        "short",
        "static",
        "strictfp",
        "super",
        "switch",
        "synchronized",
        "this",
        "throw",
        "throws",
        "transient",
        "true",
        "try",
        "void",
        "volatile",
        "while",
        "_",
    ];
    for segment in segments {
        let mut chars = segment.chars();
        let starts_with_letter = chars.next().is_some_and(|c| c.is_ascii_alphabetic());
        if !starts_with_letter || !segment.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!(
                "[project] id `{id}`: segment `{segment}` must start with a letter and contain only \
                 ASCII letters, digits and `_`"
            ));
        }
        if JAVA_RESERVED.contains(&segment) {
            return Err(format!(
                "[project] id `{id}`: segment `{segment}` is a Java keyword"
            ));
        }
    }
    Ok(())
}

/// The `config/*.ron` files next to the manifest (read-only list for now).
#[must_use]
pub fn config_files(project_root: &Path) -> Vec<String> {
    let dir = project_root.join("config");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("ron"))
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
        .collect();
    out.sort();
    out
}

/// Draws the Project Settings panel.
#[tracing::instrument(target = "regia_editor", skip_all, name = "project_settings_panel")]
pub fn project_settings_panel(
    ui: &mut egui::Ui,
    state: &mut ProjectSettingsState,
    project_root: Option<&Path>,
) {
    ui.horizontal(|ui| {
        ui.strong("Project Settings");
        match project_root {
            Some(root) => {
                ui.weak(root.display().to_string());
            }
            None => {
                ui.weak("(no project open)");
            }
        }
        if state.dirty {
            ui.colored_label(crate::theme::ui_tokens(ui.ctx()).accents.unsaved, "· unsaved");
        }
    });
    ui.separator();

    let Some(root) = project_root else {
        ui.weak("Open a project to edit its Regia.toml.");
        return;
    };
    if state.is_loaded()
        && !state.dirty
        && state.loaded_revision != crate::closure_settings::manifest_revision()
    {
        // An undo (or an agent edit) rewrote Regia.toml; nothing here is unsaved, so show the file.
        let _ = state.load(root);
    }
    if !state.is_loaded() {
        // Load lazily so external edits (the CLI, an agent) are picked up when the panel opens.
        if let Err(err) = state.load(root) {
            ui.weak(err);
            return;
        }
    }

    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.strong("[project]");
        text_row(ui, state, "Name", "name");
        text_row(ui, state, "Id", "id");
        text_row(ui, state, "Version", "version");
        int_row(ui, state, "Build number", "build_number");
        // Organization, default level picker, platforms + quality presets, input maps (ADR-2321).
        crate::project_settings_ext::extended_sections(ui, state, root);

        ui.separator();
        chunk_rules_section(ui, state);
        closure_preview_section(ui, state, root);

        // Config files (read-only today).
        ui.separator();
        ui.strong("config/");
        let files = config_files(root);
        if files.is_empty() {
            ui.weak("(no config/*.ron files)");
        } else {
            for file in files {
                ui.weak(format!("· {file}"));
            }
        }

        ui.separator();
        egui::CollapsingHeader::new("XR")
            .id_salt("project_settings_xr")
            .show(ui, |ui| {
                crate::xr_settings::xr_settings_section(ui, &mut state.xr, root);
            });

        ui.separator();
        if !state.toast.is_empty() {
            ui.weak(&state.toast);
            ui.separator();
        }
    });

    ui.horizontal(|ui| {
        if ui.button("Reload").clicked() {
            let _ = state.load(root);
        }
        if ui.add_enabled(state.dirty, egui::Button::new("Save")).clicked() {
            // Chunk settings are checked as the cook checks them (an entry root that matches
            // nothing refuses the save); the write is one undo entry (CT-26).
            match crate::closure_settings::save_checked(state, root, "", "Project Settings") {
                Ok(saved) => {
                    if !saved.verified && saved.preview.is_none() {
                        state.toast.push_str(" (entry roots not checked: no cook yet)");
                    }
                    state.manifest_undo.push(saved.undo);
                }
                Err(err) => state.toast = format!("{}: {}", err.code, err.message),
            }
        }
    });
}

/// `[content]` and the `[cook.chunks.*]` rules. Save validates them the way the cook does.
fn chunk_rules_section(ui: &mut egui::Ui, state: &mut ProjectSettingsState) {
    ui.strong("[content]");
    let mut url = state.content_base_url();
    ui.horizontal(|ui| {
        ui.label("Chunk host (base_url):");
        if ui
            .add(egui::TextEdit::singleline(&mut url).hint_text("https://cdn.example.com/game"))
            .changed()
        {
            state.set_content_base_url(&url);
        }
    });
    ui.separator();
    ui.strong("[cook.chunk_options]");
    let mut mode = state.content_mode();
    ui.horizontal(|ui| {
        ui.label("Content:");
        egui::ComboBox::from_id_salt("chunk_content_mode")
            .selected_text(mode.name())
            .show_ui(ui, |ui| {
                for option in [ContentMode::All, ContentMode::Closure] {
                    ui.selectable_value(&mut mode, option, option.name());
                }
            });
        ui.weak(match mode {
            ContentMode::All => "every cooked asset ships",
            ContentMode::Closure => "only what the startup level and entry roots reach ships",
        });
    });
    if mode != state.content_mode() {
        state.set_content_mode(mode);
    }
    ui.separator();
    ui.strong("[cook.chunks]");
    for mut rule in state.chunk_rules() {
        let name = rule.name.clone();
        let before = rule.clone();
        let mut remove = false;
        egui::CollapsingHeader::new(name.as_str())
            .id_salt(("chunk_rule", name.as_str()))
            .show(ui, |ui| {
                list_row(ui, "Include", &mut rule.include);
                list_row(ui, "Entry roots", &mut rule.entry);
                list_row(ui, "Tags", &mut rule.tags);
                list_row(ui, "Requires", &mut rule.requires);
                ui.horizontal(|ui| {
                    ui.label("Delivery:");
                    egui::ComboBox::from_id_salt(("chunk_delivery", name.as_str()))
                        .selected_text(if rule.delivery.is_empty() {
                            "default"
                        } else {
                            rule.delivery.as_str()
                        })
                        .show_ui(ui, |ui| {
                            for delivery in ["", "install", "fast_follow", "on_demand", "external"] {
                                let label = if delivery.is_empty() { "default" } else { delivery };
                                ui.selectable_value(&mut rule.delivery, delivery.to_owned(), label);
                            }
                        });
                    ui.label("Priority:");
                    let mut priority = rule.priority.unwrap_or(1);
                    if ui
                        .add(egui::DragValue::new(&mut priority).range(1..=1000))
                        .changed()
                    {
                        rule.priority = Some(priority);
                    }
                });
                remove = ui.button("Remove rule").clicked();
            });
        if remove {
            state.remove_chunk_rule(&name);
        } else if rule != before {
            state.set_chunk_rule(&rule);
        }
    }
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.new_chunk)
                .hint_text("chunk name")
                .desired_width(140.0),
        );
        if ui
            .add_enabled(!state.new_chunk.is_empty(), egui::Button::new("Add chunk"))
            .clicked()
        {
            let edit = ChunkRuleEdit {
                name: std::mem::take(&mut state.new_chunk),
                ..ChunkRuleEdit::default()
            };
            state.set_chunk_rule(&edit);
        }
    });
}

/// The content-chunk preview: what the unsaved settings would cook, computed off-thread from the
/// edited manifest (`editor.content_chunks_preview` computes the same).
fn closure_preview_section(ui: &mut egui::Ui, state: &mut ProjectSettingsState, root: &Path) {
    use crate::closure_settings::{PreviewRequest, PreviewStatus};
    let status = state.closure_preview.poll();
    ui.horizontal(|ui| {
        if ui.button("Preview chunk assignment").clicked()
            && let Some(edited) = state.manifest_text()
        {
            state.closure_preview.request(PreviewRequest {
                project: root.to_path_buf(),
                platform: String::new(),
                edited,
            });
        }
        ui.weak(status.name());
    });
    if status == PreviewStatus::Pending {
        ui.ctx().request_repaint();
    }
    if let Some(error) = state.closure_preview.error() {
        ui.colored_label(
            egui::Color32::from_rgb(220, 120, 80),
            format!("{}: {}", error.code, error.message),
        );
    }
    let Some(preview) = state.closure_preview.latest().cloned() else {
        return;
    };
    if let Some(problem) = &preview.problem {
        ui.colored_label(
            egui::Color32::from_rgb(220, 120, 80),
            format!("will not save: {}: {}", problem.code, problem.message),
        );
    }
    ui.label(format!(
        "content = {}: {} shipped, {} unshipped, {} move(s)",
        preview.content.name(),
        preview.chunk_of.len(),
        preview.unshipped.len(),
        preview.moves.len()
    ));
    for root in &preview.roots {
        let text = format!("{} = {}", root.declared_by, root.pattern);
        if root.matched {
            ui.weak(text);
        } else {
            ui.colored_label(
                egui::Color32::from_rgb(220, 120, 80),
                format!("{text} (matches nothing)"),
            );
        }
    }
    for m in preview.moves.iter().take(crate::chunks_panel::MAX_DIRECT_ASSETS) {
        ui.weak(format!(
            "{}: {} -> {}",
            m.path,
            m.from.as_deref().unwrap_or("unshipped"),
            m.to.as_deref().unwrap_or("unshipped")
        ));
    }
}

/// One comma-separated list row.
fn list_row(ui: &mut egui::Ui, label: &str, items: &mut Vec<String>) {
    let mut text = items.join(", ");
    ui.horizontal(|ui| {
        let name = ui.label(format!("{label}:"));
        if ui.text_edit_singleline(&mut text).labelled_by(name.id).changed() {
            *items = split_list(&text);
        }
    });
}

/// One string field row.
fn text_row(ui: &mut egui::Ui, state: &mut ProjectSettingsState, label: &str, key: &str) {
    let mut value = state.project_string(key);
    ui.horizontal(|ui| {
        let name = ui.label(format!("{label}:"));
        if ui.text_edit_singleline(&mut value).labelled_by(name.id).changed() {
            state.set_project_string(key, &value);
        }
    });
}

/// One integer field row.
fn int_row(ui: &mut egui::Ui, state: &mut ProjectSettingsState, label: &str, key: &str) {
    let raw = state.project_int(key).unwrap_or(1).to_string();
    let mut value = raw.clone();
    ui.horizontal(|ui| {
        let name = ui.label(format!("{label}:"));
        if ui.text_edit_singleline(&mut value).labelled_by(name.id).changed()
            && let Ok(parsed) = value.trim().parse::<i64>()
        {
            state.set_project_int(key, parsed);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_project(label: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-editor-{label}-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    /// A manifest with keys the panel does not model — they must survive a save.
    const MANIFEST: &str = r#"
[project]
name = "Sim Bridge"
id = "com.regiaengine.samples.sim_bridge"
version = "0.1.0"
build_number = 1
engine = "0.1"

[platforms.android]
min_sdk = 29
target_sdk = 35
orientation = "sensor_landscape"
abis = ["arm64-v8a", "x86_64"]
cargo_package = "sim_bridge_android"

[cook]
chunks = { base = ["**"] }
"#;

    #[test]
    fn chunk_rules_edit_keeps_other_keys_and_validate_on_save() {
        let dir = temp_project("chunk-rules");
        let plain = MANIFEST.replace("[cook]\nchunks = { base = [\"**\"] }\n", "");
        let toml = format!("{plain}\n[cook.chunks.arena]\ninclude = [\"levels/arena/**\"]\npin = true\n");
        std::fs::write(dir.join("Regia.toml"), toml).expect("manifest");
        let mut state = ProjectSettingsState::default();
        state.load(&dir).expect("load");
        let mut rules = state.chunk_rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].include, ["levels/arena/**"]);

        rules[0].delivery = "on_demand".into();
        rules[0].priority = Some(20);
        rules[0].requires = vec!["intro".into()];
        state.set_chunk_rule(&rules[0]);
        // `requires` names a chunk that has no rule yet.
        assert!(
            state
                .save(&dir)
                .expect_err("unknown chunk")
                .contains("chunk rules")
        );
        state.set_chunk_rule(&ChunkRuleEdit {
            name: "intro".into(),
            include: split_list(" a/**, b/** ,"),
            ..ChunkRuleEdit::default()
        });
        state.set_content_base_url("https://cdn.example.com/game");
        state.save(&dir).expect("save");
        let saved = std::fs::read_to_string(dir.join("Regia.toml")).expect("reread");
        let rules = ChunkRules::from_manifest_toml(&saved).expect("valid rules");
        assert_eq!(rules.rule("arena").priority, 20);
        assert_eq!(rules.rule("intro").include, ["a/**", "b/**"]);
        assert!(rules.rule("arena").pin, "keys the panel does not model stay");
        assert_eq!(rules.base_url.as_deref(), Some("https://cdn.example.com/game"));

        assert!(!state.remove_chunk_rule("nope"));
        let mut arena = state.chunk_rules().remove(0);
        arena.requires.clear();
        state.set_chunk_rule(&arena);
        assert!(state.remove_chunk_rule("intro"));
        state.set_content_base_url("");
        state.save(&dir).expect("save");
        let saved = std::fs::read_to_string(dir.join("Regia.toml")).expect("reread");
        assert!(!saved.contains("[cook.chunks.intro]"));
        assert!(!saved.contains("base_url"));

        // The old shorthand reads as include and becomes a table on the first edit.
        std::fs::write(dir.join("Regia.toml"), MANIFEST).expect("manifest");
        state.load(&dir).expect("load");
        let mut base = state.chunk_rules().remove(0);
        assert_eq!(base.include, ["**"]);
        base.priority = Some(5);
        state.set_chunk_rule(&base);
        state.save(&dir).expect("save");
        let saved = std::fs::read_to_string(dir.join("Regia.toml")).expect("reread");
        let rules = ChunkRules::from_manifest_toml(&saved).expect("valid rules");
        assert_eq!(rules.rule("base").priority, 5);
        assert_eq!(rules.rule("base").include, ["**"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn edits_save_and_unknown_keys_survive() {
        let dir = temp_project("settings");
        std::fs::write(dir.join("Regia.toml"), MANIFEST).expect("manifest");
        std::fs::create_dir_all(dir.join("config")).expect("config");
        std::fs::write(dir.join("config/input.ron"), "()").expect("config file");

        let mut state = ProjectSettingsState::default();
        state.load(&dir).expect("load");
        assert_eq!(state.project_string("name"), "Sim Bridge");
        assert_eq!(state.project_int("build_number"), Some(1));
        assert!(!state.dirty);

        // Edit through the panel's paths.
        state.set_project_string("name", "Sim Bridge 2");
        state.set_project_string("startup_level", "assets/maps/main.level.ron");
        state.set_project_int("build_number", 7);
        assert!(state.dirty);
        state.save(&dir).expect("save");

        // Unknown keys survive; modelled edits landed. (toml renders the nested cook table as
        // `[cook.chunks]`.)
        let saved = std::fs::read_to_string(dir.join("Regia.toml")).expect("reread");
        assert!(saved.contains("[platforms.android]"), "{saved}");
        assert!(saved.contains("sim_bridge_android"), "{saved}");
        assert!(saved.contains("cook.chunks"), "{saved}");
        assert!(saved.contains("Sim Bridge 2"), "{saved}");
        assert!(saved.contains("startup_level"), "{saved}");
        assert!(saved.contains("build_number = 7"), "{saved}");

        // A reload reads the saved state; config files list.
        let mut fresh = ProjectSettingsState::default();
        fresh.load(&dir).expect("reload");
        assert_eq!(fresh.project_string("name"), "Sim Bridge 2");
        assert_eq!(fresh.project_int("build_number"), Some(7));
        assert_eq!(config_files(&dir), ["input.ron"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn app_id_validation_matches_the_manifest_rules() {
        for good in ["com.example.game", "a.b", "com.Example_2.x9"] {
            assert!(validate_app_id(good).is_ok(), "{good}");
        }
        for bad in [
            "game",
            "com..x",
            "com.1x",
            "com.my-game",
            "com.example.new",
            ".com.x",
            "com.x.",
            "çom.x",
        ] {
            assert!(validate_app_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn save_refuses_invalid_modelled_fields() {
        let dir = temp_project("settings-invalid");
        std::fs::write(dir.join("Regia.toml"), MANIFEST).expect("manifest");
        let mut state = ProjectSettingsState::default();
        state.load(&dir).expect("load");
        state.set_project_string("id", "nope");
        let err = state.save(&dir).expect_err("invalid id refuses");
        assert!(err.contains("at least two segments"), "{err}");
        state.set_project_string("id", "com.regiaengine.demo");
        state.save(&dir).expect("valid id saves");
        assert!(!state.dirty);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The panel's input-map and physics-matrix writes of one frame, moved to the shared history
    /// the way the shell does at the end of each frame (`shell.rs`, after the dock draws).
    fn drain_like_the_shell(world: &mut regia_ecs::prelude::World, state: &mut ProjectSettingsState) {
        for command in std::mem::take(&mut state.input_undo) {
            crate::project_settings_ext::push_input_undo(world, command);
        }
        for command in std::mem::take(&mut state.physics_undo) {
            crate::physics_matrix::push_matrix_undo(world, command);
        }
    }

    fn panel_edits(dir: &Path) -> ProjectSettingsState {
        std::fs::create_dir_all(dir.join("config")).expect("config dir");
        let mut state = ProjectSettingsState::default();
        let (_, input) = crate::project_settings_ext::edit_input_command(
            dir,
            &[crate::project_settings_ext::InputEdit::AddAction {
                name: "Jump".into(),
                kind: "Button".into(),
            }],
        )
        .expect("input write");
        state.input_undo.push(input);
        let (_, physics) = crate::physics_matrix::edit_matrix(
            dir,
            &[crate::physics_matrix::MatrixEdit::AddLayer { name: "water".into() }],
        )
        .expect("physics write");
        state.physics_undo.push(physics.expect("one change"));
        state
    }

    /// Global undo (P-322): the Project Settings panel's input-map and physics-layer writes are
    /// entries of the shared history; the editor's Undo restores each file and Redo writes it again.
    #[test]
    fn an_edit_in_project_settings_undoes_from_the_global_stack() {
        let dir = temp_project("settings-global-undo");
        let mut world = regia_ecs::prelude::World::new();
        world.insert_resource(crate::command::UndoStack::new());
        let mut state = panel_edits(&dir);
        drain_like_the_shell(&mut world, &mut state);
        assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), 2);
        let input = dir.join("config/input.ron");
        let physics = dir.join("config/physics.ron");
        let written = std::fs::read_to_string(&physics).expect("physics");
        assert!(crate::command::undo_world(&mut world));
        assert!(
            !physics.exists(),
            "undo removes the physics file the edit created"
        );
        assert!(input.exists());
        assert!(crate::command::undo_world(&mut world));
        assert!(!input.exists(), "undo removes the input file the edit created");
        assert!(crate::command::redo_world(&mut world));
        assert!(crate::command::redo_world(&mut world));
        assert_eq!(std::fs::read_to_string(&physics).expect("physics"), written);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Global undo (P-322): the Undo History panel lists the Project Settings writes with the file
    /// each one rewrites.
    #[test]
    fn an_edit_in_project_settings_appears_in_undo_history() {
        let dir = temp_project("settings-global-history");
        let mut world = regia_ecs::prelude::World::new();
        world.insert_resource(crate::command::UndoStack::new());
        let mut state = panel_edits(&dir);
        drain_like_the_shell(&mut world, &mut state);
        let stack = world.resource::<crate::command::UndoStack>();
        let documents: Vec<Vec<String>> = stack.history().into_iter().map(|e| e.documents).collect();
        assert_eq!(
            documents,
            vec![
                vec!["config/input.ron".to_owned()],
                vec!["config/physics.ron".to_owned()]
            ]
        );
        assert!(!stack.is_dirty(), "project settings do not dirty the level");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
