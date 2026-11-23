// SPDX-License-Identifier: MPL-2.0
//! The editor's creators (`docs/specs/19-editor.md` § 2, § 5, ADR-7234): the Project Settings pages
//! for localization, packaging and audio buses, and the New script wizard. Physics layers have one
//! editor, the layer matrix of [`crate::physics_matrix`] (ADR-7233, ADR-7267); the pages' JSON
//! carries its view.
//!
//! Each page edits its file through a small model; the panel's widgets and the agent methods
//! (`editor.project_localization_edit`, `editor.project_packaging_edit`, `editor.create_script`)
//! call the same functions, so a test can drive a page headless. The localization page writes
//! `assets/localization/localization.ron` at once, as one undo step, after the same check the
//! runtime loader does; packaging keys live in `Regia.toml` and are saved with the rest of the
//! Project Settings (unknown keys kept).

use std::path::{Path, PathBuf};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::command::{EditorCommand, UndoStack};
use crate::data_panels::PanelError;
use crate::project_settings::ProjectSettingsState;
use crate::viewport_agent::Handler;

/// Where the audio bus layout lives, relative to the project root.
pub const AUDIO_CONFIG_PATH: &str = "config/audio.ron";

const HINT: &str = "see book/agents/editor-creators.md";

fn err(code: &str, message: impl Into<String>) -> PanelError {
    PanelError::new(code, message)
}

/// Writes `text` to `root/rel` atomically, creating the parent directory.
fn write_file(root: &Path, rel: &str, text: &str, code: &str) -> Result<(), PanelError> {
    let full = root.join(rel);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| err(code, format!("cannot create {}: {e}", parent.display())))?;
    }
    crate::atomic_write::write_atomic(&full, text.as_bytes())
        .map_err(|e| err(code, format!("cannot write {}: {e}", full.display())))
}

/// One config file write on the editor's undo stack: the text before (`None` = no file) and after.
struct ConfigFileEdit {
    root: PathBuf,
    rel: &'static str,
    before: Option<String>,
    after: String,
}

impl ConfigFileEdit {
    fn put(&self, text: Option<&str>) {
        let full = self.root.join(self.rel);
        let result = match text {
            Some(text) => crate::atomic_write::write_atomic(&full, text.as_bytes()),
            None => std::fs::remove_file(&full).map_err(|e| e.to_string()),
        };
        if let Err(e) = result {
            tracing::warn!(target: "regia_editor", "undo of {}: {e}", self.rel);
        }
    }
}

impl EditorCommand for ConfigFileEdit {
    fn apply(&mut self, _world: &mut World) {
        self.put(Some(&self.after));
    }
    fn revert(&mut self, _world: &mut World) {
        self.put(self.before.as_deref());
    }
    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }
    fn changes_level_state(&self) -> bool {
        false
    }
    fn description(&self) -> String {
        format!("Edit {}", self.rel)
    }
}

/// Writes `after` to `rel` and pushes the undo entry. Returns whether the file changed.
fn write_undoable(
    world: &mut World,
    root: &Path,
    rel: &'static str,
    after: String,
    code: &str,
) -> Result<bool, PanelError> {
    let before = std::fs::read_to_string(root.join(rel)).ok();
    if before.as_deref() == Some(after.as_str()) {
        return Ok(false);
    }
    write_file(root, rel, &after, code)?;
    let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    stack.seal();
    stack.push(Box::new(ConfigFileEdit {
        root: root.to_path_buf(),
        rel,
        before,
        after,
    }));
    stack.seal();
    Ok(true)
}

// ---------------------------------------------------------------------------------------------
// Localization
// ---------------------------------------------------------------------------------------------

/// Reads `localization.ron` (its defaults when the project has none).
///
/// # Errors
///
/// `localization_parse` when the file does not load.
pub fn load_localization(root: &Path) -> Result<regia_ui::loc_config::LocalizationConfig, PanelError> {
    let path = root.join(regia_ui::loc_config::LOCALIZATION_CONFIG_PATH);
    match std::fs::read_to_string(&path) {
        Ok(text) => regia_ui::loc_config::LocalizationConfig::from_ron(&text)
            .map_err(|e| err("localization_parse", e.to_string())),
        Err(_) => Ok(regia_ui::loc_config::LocalizationConfig::default()),
    }
}

/// One Localization page edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalizationEdit {
    /// Set the source locale.
    Source(String),
    /// Add a shipped locale.
    AddLocale(String),
    /// Remove a shipped locale.
    RemoveLocale(String),
}

/// Applies `edits` to `localization.ron` (created when missing), all or nothing, as one undo step.
///
/// # Errors
///
/// `localization_bad_locale` for a tag that is not BCP 47, a duplicate or a missing locale;
/// `localization_parse` for an unreadable file; nothing is written.
pub fn edit_localization(
    world: &mut World,
    root: &Path,
    edits: &[LocalizationEdit],
) -> Result<regia_ui::loc_config::LocalizationConfig, PanelError> {
    let mut config = load_localization(root)?;
    for edit in edits {
        match edit {
            LocalizationEdit::Source(tag) => config.source = tag.trim().to_owned(),
            LocalizationEdit::AddLocale(tag) => {
                let tag = tag.trim().to_owned();
                if config.locales.contains(&tag) {
                    return Err(err(
                        "localization_bad_locale",
                        format!("`{tag}` is already listed"),
                    ));
                }
                config.locales.push(tag);
            }
            LocalizationEdit::RemoveLocale(tag) => {
                let before = config.locales.len();
                config.locales.retain(|l| l != tag.trim());
                if config.locales.len() == before {
                    return Err(err("localization_bad_locale", format!("`{tag}` is not listed")));
                }
            }
        }
    }
    let text = ron::ser::to_string_pretty(&config, regia_core::ron_pretty())
        .map_err(|e| err("localization_parse", e.to_string()))?;
    // The loader's check (versions, BCP 47 tags) before anything is written.
    regia_ui::loc_config::LocalizationConfig::from_ron(&text)
        .map_err(|e| err("localization_bad_locale", e.to_string()))?;
    write_undoable(
        world,
        root,
        regia_ui::loc_config::LOCALIZATION_CONFIG_PATH,
        format!("{text}\n"),
        "localization_io",
    )?;
    Ok(config)
}

fn localization_json(config: &regia_ui::loc_config::LocalizationConfig) -> Value {
    json!({
        "path": regia_ui::loc_config::LOCALIZATION_CONFIG_PATH,
        "source": config.source,
        "locales": config.locales,
    })
}

// ---------------------------------------------------------------------------------------------
// Packaging (Regia.toml)
// ---------------------------------------------------------------------------------------------

/// Android orientations `regia package` accepts, in Regia.toml spelling.
pub const ORIENTATIONS: &[&str] = &[
    "sensor_landscape",
    "landscape",
    "portrait",
    "sensor_portrait",
    "sensor",
];

/// Linux package formats `regia package` builds.
pub const LINUX_FORMATS: &[&str] = &["tar", "dir", "appimage"];

/// The packaging keys the page edits (`[platforms.android]`, `[platforms.linux]`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PackagingPage {
    /// `minSdk`.
    pub min_sdk: i64,
    /// `targetSdk`.
    pub target_sdk: i64,
    /// Screen orientation.
    pub orientation: String,
    /// Android ABIs.
    pub abis: Vec<String>,
    /// Extra Android permissions.
    pub permissions: Vec<String>,
    /// Release keystore reference (`env:VAR` or a path).
    pub keystore: String,
    /// Keystore password reference (`env:VAR`).
    pub keystore_password: String,
    /// Key alias.
    pub key_alias: String,
    /// Key password reference (`env:VAR`).
    pub key_password: String,
    /// Linux package formats.
    pub linux_formats: Vec<String>,
}

fn table_at<'a>(state: &'a ProjectSettingsState, path: &[&str]) -> Option<&'a toml::Table> {
    let mut table = state.manifest()?;
    for key in path {
        table = table.get(*key)?.as_table()?;
    }
    Some(table)
}

fn strings(value: Option<&toml::Value>) -> Vec<String> {
    value
        .and_then(toml::Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

impl PackagingPage {
    /// Reads the page from a loaded manifest, with `regia package`'s defaults for absent keys.
    #[must_use]
    pub fn read(state: &ProjectSettingsState) -> Self {
        let android = table_at(state, &["platforms", "android"]);
        let get = |key: &str| android.and_then(|t| t.get(key));
        let text = |key: &str| get(key).and_then(toml::Value::as_str).unwrap_or("").to_owned();
        let abis = strings(get("abis"));
        Self {
            min_sdk: get("min_sdk").and_then(toml::Value::as_integer).unwrap_or(29),
            target_sdk: get("target_sdk").and_then(toml::Value::as_integer).unwrap_or(35),
            orientation: get("orientation")
                .and_then(toml::Value::as_str)
                .unwrap_or("sensor_landscape")
                .to_owned(),
            abis: if abis.is_empty() {
                vec!["arm64-v8a".to_owned()]
            } else {
                abis
            },
            permissions: strings(get("permissions")),
            keystore: text("keystore"),
            keystore_password: text("keystore_password"),
            key_alias: text("key_alias"),
            key_password: text("key_password"),
            linux_formats: strings(table_at(state, &["platforms", "linux"]).and_then(|t| t.get("formats"))),
        }
    }

    /// Checks the page: SDK levels, orientation, formats, and that passwords are `env:` references
    /// (a literal password in `Regia.toml` would be committed with the project).
    ///
    /// # Errors
    ///
    /// `packaging_invalid` or `packaging_secret_literal`.
    pub fn validate(&self) -> Result<(), PanelError> {
        if !(21..=99).contains(&self.min_sdk) || !(21..=99).contains(&self.target_sdk) {
            return Err(err(
                "packaging_invalid",
                "min_sdk and target_sdk must be Android API levels (21..=99)",
            ));
        }
        if self.min_sdk > self.target_sdk {
            return Err(err("packaging_invalid", "min_sdk must not be above target_sdk"));
        }
        if !ORIENTATIONS.contains(&self.orientation.as_str()) {
            return Err(err(
                "packaging_invalid",
                format!(
                    "orientation `{}` is not one of {}",
                    self.orientation,
                    ORIENTATIONS.join(", ")
                ),
            ));
        }
        if self.abis.is_empty() {
            return Err(err("packaging_invalid", "at least one Android ABI is required"));
        }
        if let Some(bad) = self
            .linux_formats
            .iter()
            .find(|f| !LINUX_FORMATS.contains(&f.as_str()))
        {
            return Err(err(
                "packaging_invalid",
                format!("linux format `{bad}` is not one of {}", LINUX_FORMATS.join(", ")),
            ));
        }
        for (key, value) in [
            ("keystore_password", &self.keystore_password),
            ("key_password", &self.key_password),
        ] {
            let value = value.trim();
            if !value.is_empty() && !value.starts_with("env:") {
                return Err(err(
                    "packaging_secret_literal",
                    format!("`{key}` must be an env:VAR reference, not the password itself"),
                ));
            }
        }
        Ok(())
    }

    /// Writes the page into the manifest (other keys untouched) and marks it dirty.
    ///
    /// # Errors
    ///
    /// The [`Self::validate`] codes, or `packaging_invalid` when no manifest is loaded.
    pub fn write(&self, state: &mut ProjectSettingsState) -> Result<(), PanelError> {
        self.validate()?;
        let table = state
            .manifest_mut()
            .ok_or_else(|| err("packaging_invalid", "no manifest is loaded"))?;
        let sub = |table: &mut toml::Table, key: &str| -> Result<(), PanelError> {
            if !table.contains_key(key) {
                table.insert(key.to_owned(), toml::Value::Table(toml::Table::new()));
            }
            if table.get(key).is_some_and(toml::Value::is_table) {
                Ok(())
            } else {
                Err(err(
                    "packaging_invalid",
                    format!("`{key}` in Regia.toml is not a table"),
                ))
            }
        };
        sub(table, "platforms")?;
        let platforms = table
            .get_mut("platforms")
            .and_then(toml::Value::as_table_mut)
            .ok_or_else(|| err("packaging_invalid", "`platforms` is not a table"))?;
        sub(platforms, "android")?;
        let list =
            |items: &[String]| toml::Value::Array(items.iter().cloned().map(toml::Value::String).collect());
        if let Some(android) = platforms.get_mut("android").and_then(toml::Value::as_table_mut) {
            android.insert("min_sdk".into(), toml::Value::Integer(self.min_sdk));
            android.insert("target_sdk".into(), toml::Value::Integer(self.target_sdk));
            android.insert(
                "orientation".into(),
                toml::Value::String(self.orientation.clone()),
            );
            android.insert("abis".into(), list(&self.abis));
            android.insert("permissions".into(), list(&self.permissions));
            for (key, value) in [
                ("keystore", &self.keystore),
                ("keystore_password", &self.keystore_password),
                ("key_alias", &self.key_alias),
                ("key_password", &self.key_password),
            ] {
                if value.trim().is_empty() {
                    android.remove(key);
                } else {
                    android.insert(key.into(), toml::Value::String(value.trim().to_owned()));
                }
            }
        }
        if !self.linux_formats.is_empty() {
            sub(platforms, "linux")?;
            if let Some(linux) = platforms.get_mut("linux").and_then(toml::Value::as_table_mut) {
                linux.insert("formats".into(), list(&self.linux_formats));
            }
        }
        state.mark_dirty();
        Ok(())
    }

    /// The JSON view.
    #[must_use]
    pub fn json(&self) -> Value {
        json!({
            "android": {
                "min_sdk": self.min_sdk,
                "target_sdk": self.target_sdk,
                "orientation": self.orientation,
                "abis": self.abis,
                "permissions": self.permissions,
                "keystore": self.keystore,
                "keystore_password": self.keystore_password,
                "key_alias": self.key_alias,
                "key_password": self.key_password,
            },
            "linux": { "formats": self.linux_formats },
        })
    }
}

/// Audit ceilings stored in `[package_hygiene]` (ADR-8244).
///
/// The defaults match `Policy::DEFAULT_MAX_MEMORY_BYTES`, `Policy::DEFAULT_SCAN_WINDOW_BYTES`,
/// `Policy::DEFAULT_MAX_MATCHER_BYTES` and `Policy::DEFAULT_MAX_WORK` in `regia_cli`. This crate
/// does not depend on `regia_cli`. A hygiene save writes only these integers and leaves
/// `forbidden_strings`, `deny_terms` and `allow` untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HygieneLimits {
    /// Peak bytes charged for scan buffers and decoded entries.
    pub max_memory_bytes: u64,
    /// Maximum bytes of one file or decompressed pak entry.
    pub max_file_bytes: u64,
    /// Maximum total scanned file and decompressed-entry bytes.
    pub max_total_bytes: u64,
    /// Bytes the audit may present to matchers.
    pub max_work: u64,
    /// Window used to stream literals and bounded patterns.
    pub scan_window_bytes: u64,
    /// Sum of per-rule compiled-matcher reservations.
    pub max_matcher_bytes: u64,
}

impl Default for HygieneLimits {
    fn default() -> Self {
        Self {
            max_memory_bytes: Self::DEFAULT_MAX_MEMORY_BYTES,
            max_file_bytes: Self::DEFAULT_MAX_FILE_BYTES,
            max_total_bytes: Self::DEFAULT_MAX_TOTAL_BYTES,
            max_work: Self::DEFAULT_MAX_WORK,
            scan_window_bytes: Self::DEFAULT_SCAN_WINDOW_BYTES,
            max_matcher_bytes: Self::DEFAULT_MAX_MATCHER_BYTES,
        }
    }
}

impl HygieneLimits {
    /// Default peak resident bytes (32 MiB). Must match ADR-8244.
    pub const DEFAULT_MAX_MEMORY_BYTES: u64 = 32 << 20;
    /// Default per-file cap (1 GiB). Must match the package-hygiene policy default.
    pub const DEFAULT_MAX_FILE_BYTES: u64 = 1 << 30;
    /// Default total scan cap (64 GiB). Must match the package-hygiene policy default.
    pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 64 << 30;
    /// Default matcher-work budget (1 TiB). Must match ADR-8244.
    pub const DEFAULT_MAX_WORK: u64 = 1 << 40;
    /// Default streaming window (1 MiB). Must match ADR-8244.
    pub const DEFAULT_SCAN_WINDOW_BYTES: u64 = 1 << 20;
    /// Default compiled-matcher reservation (8 MiB). Must match ADR-8244.
    pub const DEFAULT_MAX_MATCHER_BYTES: u64 = 8 << 20;

    /// Reads effective limits. Absent keys use the defaults. A present non-positive or
    /// non-integer value reads as `0` so validation can show it.
    ///
    /// # Errors
    ///
    /// `bad_settings` when `[package_hygiene]` is present and is not a table.
    pub fn read(state: &ProjectSettingsState) -> Result<Self, PanelError> {
        let Some(root) = state.manifest() else {
            return Ok(Self::default());
        };
        let Some(value) = root.get("package_hygiene") else {
            return Ok(Self::default());
        };
        let table = value
            .as_table()
            .ok_or_else(|| err("bad_settings", "`[package_hygiene]` in Regia.toml is not a table"))?;
        Ok(Self {
            max_memory_bytes: read_limit(table, "max_memory_bytes", Self::DEFAULT_MAX_MEMORY_BYTES),
            max_file_bytes: read_limit(table, "max_file_bytes", Self::DEFAULT_MAX_FILE_BYTES),
            max_total_bytes: read_limit(table, "max_total_bytes", Self::DEFAULT_MAX_TOTAL_BYTES),
            max_work: read_limit(table, "max_work", Self::DEFAULT_MAX_WORK),
            scan_window_bytes: read_limit(table, "scan_window_bytes", Self::DEFAULT_SCAN_WINDOW_BYTES),
            max_matcher_bytes: read_limit(table, "max_matcher_bytes", Self::DEFAULT_MAX_MATCHER_BYTES),
        })
    }

    /// Checks that every ceiling is a positive integer TOML can store and that the scan window
    /// fits in the memory budget.
    ///
    /// # Errors
    ///
    /// `bad_settings` for `0`, a value above `i64::MAX`, or a window larger than memory.
    pub fn validate(&self) -> Result<(), PanelError> {
        for (key, value) in self.pairs() {
            if value == 0 || value > i64::MAX as u64 {
                return Err(err(
                    "bad_settings",
                    format!("`{key}` must be a positive integer no larger than {}", i64::MAX),
                ));
            }
        }
        if self.scan_window_bytes > self.max_memory_bytes {
            return Err(err(
                "bad_settings",
                "scan_window_bytes must not exceed max_memory_bytes",
            ));
        }
        Ok(())
    }

    /// Applies a partial `{ max_memory_bytes?, ... }` object.
    ///
    /// # Errors
    ///
    /// `bad_settings` for an unknown key, a non-integer, `0`, or a value above `i64::MAX`.
    pub fn apply(&mut self, patch: &Value) -> Result<(), PanelError> {
        let object = patch.as_object().ok_or_else(|| {
            err(
                "bad_settings",
                "`package_hygiene` must be an object of positive integers",
            )
        })?;
        for (key, value) in object {
            let slot = self.slot(key)?;
            let Some(number) = value.as_u64() else {
                return Err(err(
                    "bad_settings",
                    format!("`package_hygiene.{key}` must be a positive integer"),
                ));
            };
            if number == 0 || number > i64::MAX as u64 {
                return Err(err(
                    "bad_settings",
                    format!(
                        "`package_hygiene.{key}` must be a positive integer no larger than {}",
                        i64::MAX
                    ),
                ));
            }
            *slot = number;
        }
        Ok(())
    }

    /// Writes the six integers into `[package_hygiene]`, creating the table when needed.
    /// Other keys in that table stay as they are.
    ///
    /// # Errors
    ///
    /// [`Self::validate`], or `bad_settings` when no manifest is loaded or the section is not a table.
    pub fn write(&self, state: &mut ProjectSettingsState) -> Result<(), PanelError> {
        self.validate()?;
        let table = state
            .manifest_mut()
            .ok_or_else(|| err("bad_settings", "no manifest is loaded"))?;
        if !table.contains_key("package_hygiene") {
            table.insert(
                "package_hygiene".to_owned(),
                toml::Value::Table(toml::Table::new()),
            );
        }
        let section = table
            .get_mut("package_hygiene")
            .and_then(toml::Value::as_table_mut)
            .ok_or_else(|| err("bad_settings", "`[package_hygiene]` is not a table"))?;
        for (key, value) in self.pairs() {
            let stored = i64::try_from(value)
                .map_err(|_| err("bad_settings", format!("`{key}` does not fit in a TOML integer")))?;
            section.insert(key.to_owned(), toml::Value::Integer(stored));
        }
        state.mark_dirty();
        Ok(())
    }

    /// The JSON view of the effective limits.
    #[must_use]
    pub fn json(&self) -> Value {
        json!({
            "max_memory_bytes": self.max_memory_bytes,
            "max_file_bytes": self.max_file_bytes,
            "max_total_bytes": self.max_total_bytes,
            "max_work": self.max_work,
            "scan_window_bytes": self.scan_window_bytes,
            "max_matcher_bytes": self.max_matcher_bytes,
        })
    }

    fn pairs(&self) -> [(&'static str, u64); 6] {
        [
            ("max_memory_bytes", self.max_memory_bytes),
            ("max_file_bytes", self.max_file_bytes),
            ("max_total_bytes", self.max_total_bytes),
            ("max_work", self.max_work),
            ("scan_window_bytes", self.scan_window_bytes),
            ("max_matcher_bytes", self.max_matcher_bytes),
        ]
    }

    fn slot(&mut self, key: &str) -> Result<&mut u64, PanelError> {
        match key {
            "max_memory_bytes" => Ok(&mut self.max_memory_bytes),
            "max_file_bytes" => Ok(&mut self.max_file_bytes),
            "max_total_bytes" => Ok(&mut self.max_total_bytes),
            "max_work" => Ok(&mut self.max_work),
            "scan_window_bytes" => Ok(&mut self.scan_window_bytes),
            "max_matcher_bytes" => Ok(&mut self.max_matcher_bytes),
            _ => Err(err(
                "bad_settings",
                format!("`package_hygiene.{key}` is not a hygiene limit"),
            )),
        }
    }
}

fn read_limit(table: &toml::Table, key: &str, default: u64) -> u64 {
    match table.get(key) {
        None => default,
        Some(toml::Value::Integer(value)) => u64::try_from(*value).unwrap_or(0),
        Some(_) => 0,
    }
}

/// Applies a packaging JSON patch (`{ android: {...}, linux: { formats } }`) to `page`.
///
/// # Errors
///
/// `packaging_invalid` for an unknown key or a wrong type.
pub fn patch_packaging(page: &mut PackagingPage, patch: &Value) -> Result<(), PanelError> {
    fn bad(key: &str) -> PanelError {
        err(
            "packaging_invalid",
            format!("`{key}` is not a packaging key or has the wrong type"),
        )
    }
    fn strs(v: &Value, key: &str) -> Result<Vec<String>, PanelError> {
        v.as_array()
            .ok_or_else(|| bad(key))?
            .iter()
            .map(|s| s.as_str().map(str::to_owned).ok_or_else(|| bad(key)))
            .collect()
    }
    fn text(v: &Value, key: &str) -> Result<String, PanelError> {
        v.as_str().map(str::to_owned).ok_or_else(|| bad(key))
    }
    let object = patch.as_object().ok_or_else(|| bad("packaging"))?;
    for (section, body) in object {
        let section = section.as_str();
        let body = body.as_object().ok_or_else(|| bad(section))?;
        for (key, v) in body {
            let key = key.as_str();
            match (section, key) {
                ("android", "min_sdk") => page.min_sdk = v.as_i64().ok_or_else(|| bad(key))?,
                ("android", "target_sdk") => page.target_sdk = v.as_i64().ok_or_else(|| bad(key))?,
                ("android", "orientation") => page.orientation = text(v, key)?,
                ("android", "abis") => page.abis = strs(v, key)?,
                ("android", "permissions") => page.permissions = strs(v, key)?,
                ("android", "keystore") => page.keystore = text(v, key)?,
                ("android", "keystore_password") => page.keystore_password = text(v, key)?,
                ("android", "key_alias") => page.key_alias = text(v, key)?,
                ("android", "key_password") => page.key_password = text(v, key)?,
                ("linux", "formats") => page.linux_formats = strs(v, key)?,
                _ => return Err(bad(&format!("{section}.{key}"))),
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Audio buses (read here; the Audio Mixer edits them)
// ---------------------------------------------------------------------------------------------

/// The bus names of `config/audio.ron` (the default tree when the file is missing), parent first.
///
/// # Errors
///
/// `audio_buses_parse` when the file does not parse.
pub fn audio_buses(root: &Path) -> Result<Vec<String>, PanelError> {
    let layout = match std::fs::read_to_string(root.join(AUDIO_CONFIG_PATH)) {
        Ok(text) => {
            regia_audio::BusLayout::from_ron(&text).map_err(|e| err("audio_buses_parse", e.to_string()))?
        }
        Err(_) => regia_audio::BusLayout::default_tree(),
    };
    Ok(layout.buses.iter().map(|b| b.name.clone()).collect())
}

/// The four pages as JSON (each page's error in place of its view when its file is broken);
/// `editor.project_settings_get` carries this as `pages`.
#[must_use]
pub fn pages_json(state: &ProjectSettingsState, root: &Path) -> Value {
    // The physics layer matrix's own view (ADR-7267: one editor for config/physics.ron).
    let physics = match crate::physics_matrix::load_matrix(root) {
        Ok((file, exists)) => crate::physics_matrix::matrix_json(&file, exists),
        Err(e) => json!({ "code": e.code, "message": e.message }),
    };
    let localization = match load_localization(root) {
        Ok(config) => localization_json(&config),
        Err(e) => e.json(),
    };
    let audio = match audio_buses(root) {
        Ok(buses) => json!({ "path": AUDIO_CONFIG_PATH, "buses": buses }),
        Err(e) => e.json(),
    };
    let package_hygiene = match HygieneLimits::read(state) {
        Ok(limits) => limits.json(),
        Err(error) => error.json(),
    };
    json!({
        "physics": physics,
        "localization": localization,
        "packaging": PackagingPage::read(state).json(),
        "package_hygiene": package_hygiene,
        "audio": audio,
    })
}

// ---------------------------------------------------------------------------------------------
// The pages' UI
// ---------------------------------------------------------------------------------------------

/// The text boxes of the pages (the panel's per-frame scratch state).
#[derive(Resource, Debug, Default, Clone)]
pub struct PagesUiState {
    /// The localization "Add locale" box.
    pub new_locale: String,
    /// The last page error or action.
    pub status: String,
    /// Package-hygiene rule and allowance fields (ADR-8399).
    pub hygiene: crate::hygiene_policy::HygieneForm,
}

/// Draws the Localization, Packaging and Audio pages under the Project Settings panel (physics
/// layers are the panel's own layer matrix section, `crate::physics_matrix::matrix_section`).
#[tracing::instrument(target = "regia_editor", skip_all, name = "project_settings_pages")]
pub fn project_settings_pages(
    ui: &mut egui::Ui,
    world: &mut World,
    state: &mut ProjectSettingsState,
    root: &Path,
) {
    crate::debug_colors::panel(ui, world);
    let mut scratch = world.remove_resource::<PagesUiState>().unwrap_or_default();
    egui::CollapsingHeader::new("Localization")
        .default_open(false)
        .show(ui, |ui| localization_page_ui(ui, world, root, &mut scratch));
    egui::CollapsingHeader::new("Packaging")
        .default_open(false)
        .show(ui, |ui| {
            packaging_page_ui(ui, state, &mut scratch);
            crate::hygiene_policy::policy_section(
                ui,
                world,
                state,
                root,
                &mut scratch.hygiene,
                &mut scratch.status,
            );
        });
    egui::CollapsingHeader::new("Audio buses")
        .default_open(false)
        .show(ui, |ui| match audio_buses(root) {
            Ok(buses) => {
                ui.label(format!("{AUDIO_CONFIG_PATH}: {}", buses.join(", ")));
                if ui.button("Open Audio Mixer").clicked() {
                    crate::editor_layout::LayoutRequests::push(
                        world,
                        crate::editor_layout::LayoutRequest::Open(crate::shell::Panel::AudioMixer),
                    );
                }
            }
            Err(e) => e.draw(ui),
        });
    if !scratch.status.is_empty() {
        ui.weak(&scratch.status);
    }
    world.insert_resource(scratch);
}

fn localization_page_ui(ui: &mut egui::Ui, world: &mut World, root: &Path, scratch: &mut PagesUiState) {
    let config = match load_localization(root) {
        Ok(config) => config,
        Err(e) => {
            e.draw(ui);
            return;
        }
    };
    let mut edits = Vec::new();
    ui.horizontal(|ui| {
        ui.label("Source locale");
        let mut source = config.source.clone();
        if ui.text_edit_singleline(&mut source).lost_focus() && source != config.source {
            edits.push(LocalizationEdit::Source(source));
        }
    });
    ui.label("Shipped locales");
    for locale in &config.locales {
        ui.horizontal(|ui| {
            ui.label(locale);
            if ui.small_button("Remove").clicked() {
                edits.push(LocalizationEdit::RemoveLocale(locale.clone()));
            }
        });
    }
    ui.horizontal(|ui| {
        ui.text_edit_singleline(&mut scratch.new_locale);
        if ui.button("Add locale").clicked() {
            edits.push(LocalizationEdit::AddLocale(scratch.new_locale.clone()));
            scratch.new_locale.clear();
        }
    });
    if !edits.is_empty() {
        scratch.status = match edit_localization(world, root, &edits) {
            Ok(_) => format!("saved {}", regia_ui::loc_config::LOCALIZATION_CONFIG_PATH),
            Err(e) => format!("{} ({})", e.message, e.code),
        };
    }
}

fn packaging_page_ui(ui: &mut egui::Ui, state: &mut ProjectSettingsState, scratch: &mut PagesUiState) {
    if !state.is_loaded() {
        ui.weak("Load the project settings first.");
        return;
    }
    let before = PackagingPage::read(state);
    let mut page = before.clone();
    egui::Grid::new("packaging").num_columns(2).show(ui, |ui| {
        ui.label("Android min SDK");
        ui.add(egui::DragValue::new(&mut page.min_sdk).range(21..=99));
        ui.end_row();
        ui.label("Android target SDK");
        ui.add(egui::DragValue::new(&mut page.target_sdk).range(21..=99));
        ui.end_row();
        ui.label("Orientation");
        egui::ComboBox::from_id_salt("packaging-orientation")
            .selected_text(page.orientation.clone())
            .show_ui(ui, |ui| {
                for o in ORIENTATIONS {
                    ui.selectable_value(&mut page.orientation, (*o).to_owned(), *o);
                }
            });
        ui.end_row();
        for (label, value) in [
            ("Keystore (env:VAR or path)", &mut page.keystore),
            ("Keystore password (env:VAR)", &mut page.keystore_password),
            ("Key alias", &mut page.key_alias),
            ("Key password (env:VAR)", &mut page.key_password),
        ] {
            ui.label(label);
            ui.text_edit_singleline(value);
            ui.end_row();
        }
        ui.label("Linux formats");
        ui.horizontal(|ui| {
            for format in LINUX_FORMATS {
                let mut on = page.linux_formats.iter().any(|f| f.as_str() == *format);
                if ui.checkbox(&mut on, *format).changed() {
                    page.linux_formats.retain(|f| f.as_str() != *format);
                    if on {
                        page.linux_formats.push((*format).to_owned());
                    }
                }
            }
        });
        ui.end_row();
    });
    ui.weak(format!("ABIs: {}", page.abis.join(", ")));
    if page != before {
        scratch.status = match page.write(state) {
            Ok(()) => "packaging changed; Save writes Regia.toml".to_owned(),
            Err(e) => format!("{} ({})", e.message, e.code),
        };
    }
    let hygiene_before = match HygieneLimits::read(state) {
        Ok(limits) => limits,
        Err(error) => {
            scratch.status = format!("{} ({})", error.message, error.code);
            return;
        }
    };
    let mut hygiene = hygiene_before.clone();
    let limit_range = 0..=(i64::MAX as u64);
    egui::Grid::new("package-hygiene").num_columns(2).show(ui, |ui| {
        for (label, value) in [
            ("Hygiene memory budget", &mut hygiene.max_memory_bytes),
            ("Hygiene file bytes", &mut hygiene.max_file_bytes),
            ("Hygiene total bytes", &mut hygiene.max_total_bytes),
            ("Hygiene work budget", &mut hygiene.max_work),
            ("Hygiene scan window", &mut hygiene.scan_window_bytes),
            ("Hygiene matcher budget", &mut hygiene.max_matcher_bytes),
        ] {
            ui.label(label);
            ui.add(egui::DragValue::new(value).range(limit_range.clone()));
            ui.end_row();
        }
    });
    if hygiene != hygiene_before {
        scratch.status = match hygiene.write(state) {
            Ok(()) => "package hygiene limits changed; Save writes Regia.toml".to_owned(),
            Err(error) => format!("{} ({})", error.message, error.code),
        };
    }
}

// ---------------------------------------------------------------------------------------------
// New script wizard
// ---------------------------------------------------------------------------------------------

/// The New script wizard's templates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScriptTemplate {
    /// A module with a doc comment only.
    #[default]
    Empty,
    /// A reflected component (`#[derive(Component, Reflect, Default)]`) the game registers.
    Component,
}

impl ScriptTemplate {
    /// Every template, in the wizard's order.
    pub const ALL: [Self; 2] = [Self::Empty, Self::Component];

    /// The protocol name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Component => "component",
        }
    }

    /// Parses the protocol name.
    ///
    /// # Errors
    ///
    /// `script_bad_template`.
    pub fn parse(name: &str) -> Result<Self, PanelError> {
        Self::ALL.into_iter().find(|t| t.as_str() == name).ok_or_else(|| {
            err(
                "script_bad_template",
                format!("unknown script template `{name}` (empty, component)"),
            )
        })
    }

    /// The file body for module `module`.
    #[must_use]
    pub fn body(self, module: &str) -> String {
        match self {
            Self::Empty => format!("//! Script `{module}`.\n\n// Gameplay for this script lives here.\n"),
            Self::Component => {
                let type_name: String = module
                    .split('_')
                    .filter(|p| !p.is_empty())
                    .map(|p| {
                        let mut chars = p.chars();
                        chars
                            .next()
                            .map(|c| c.to_ascii_uppercase().to_string() + chars.as_str())
                            .unwrap_or_default()
                    })
                    .collect();
                format!(
                    "//! Script `{module}`.\n\nuse regia::prelude::*;\n\n/// The `{module}` script's data. Register it in the game plugin with\n/// `app.register_type::<{type_name}>()` so the editor can add it to entities.\n#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]\n#[reflect(Component, Default)]\npub struct {type_name} {{}}\n"
                )
            }
        }
    }
}

/// The New script wizard (Tools ▸ New script…): name, template, Create.
#[derive(Resource, Debug, Default, Clone)]
pub struct ScriptWizard {
    /// Whether the wizard window is shown.
    pub open: bool,
    /// The script name typed so far.
    pub name: String,
    /// The chosen template.
    pub template: ScriptTemplate,
    /// The last error or the created file.
    pub status: String,
    /// The file the last Create wrote.
    pub created: Option<PathBuf>,
}

/// The wizard's form; Create calls [`crate::scripts::create_script_from`] (the same code as
/// `editor.create_script`). Returns the created file on the frame Create succeeds.
pub fn script_wizard_ui(
    ui: &mut egui::Ui,
    wizard: &mut ScriptWizard,
    root: Option<&Path>,
) -> Option<PathBuf> {
    ui.horizontal(|ui| {
        ui.label("Name");
        ui.text_edit_singleline(&mut wizard.name);
    });
    ui.horizontal(|ui| {
        ui.label("Template");
        for template in ScriptTemplate::ALL {
            ui.selectable_value(&mut wizard.template, template, template.as_str());
        }
    });
    let module = crate::scaffold::crate_name_for(&wizard.name).ok();
    match &module {
        Some(module) => ui.weak(format!(
            "game/src/scripts/{module}.rs, registered at the REGIA:SCRIPTS marker"
        )),
        None => ui.weak("Type a name (letters, digits, spaces)."),
    };
    let mut created = None;
    ui.horizontal(|ui| {
        let ready = module.is_some() && root.is_some();
        if ui.add_enabled(ready, egui::Button::new("Create")).clicked()
            && let Some(root) = root
        {
            match crate::scripts::create_script_from(root, &wizard.name, wizard.template) {
                Ok(path) => {
                    wizard.status = format!("created {}", path.display());
                    wizard.created = Some(path.clone());
                    wizard.name.clear();
                    created = Some(path);
                }
                Err(message) => wizard.status = message,
            }
        }
        if ui.button("Close").clicked() {
            wizard.open = false;
        }
    });
    if !wizard.status.is_empty() {
        ui.weak(&wizard.status);
    }
    created
}

/// Shows the wizard window when it is open (the shell calls this every frame).
pub fn script_wizard_window(ctx: &egui::Context, world: &mut World) {
    let Some(mut wizard) = world.remove_resource::<ScriptWizard>() else {
        return;
    };
    if wizard.open {
        let root = world
            .get_resource::<crate::agent_ext::EditorProject>()
            .and_then(|p| p.root.clone());
        let mut open = true;
        egui::Window::new("New script")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                script_wizard_ui(ui, &mut wizard, root.as_deref());
            });
        wizard.open &= open;
    }
    world.insert_resource(wizard);
}

// ---------------------------------------------------------------------------------------------
// Agent protocol
// ---------------------------------------------------------------------------------------------

fn root_of(world: &World) -> Result<PathBuf, RpcError> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or_else(|| err("no_project", "no project is open").rpc("open one with editor.open_project"))
}

fn str_field<'a>(op: &'a Value, key: &str, code: &str) -> Result<&'a str, RpcError> {
    op.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| err(code, format!("`{key}` (string) is required")).rpc(HINT))
}

/// `editor.project_localization_edit { edits }`.
fn project_localization_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = root_of(world)?;
    let ops = params
        .get("edits")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut edits = Vec::with_capacity(ops.len());
    for op in &ops {
        let tag = str_field(op, "locale", "localization_bad_edit")?.to_owned();
        edits.push(match op.get("op").and_then(Value::as_str) {
            Some("source") => LocalizationEdit::Source(tag),
            Some("add_locale") => LocalizationEdit::AddLocale(tag),
            Some("remove_locale") => LocalizationEdit::RemoveLocale(tag),
            _ => {
                return Err(err(
                    "localization_bad_edit",
                    "`op` must be source, add_locale or remove_locale",
                )
                .rpc(HINT));
            }
        });
    }
    let config = edit_localization(world, &root, &edits).map_err(|e| e.rpc(HINT))?;
    Ok(json!({ "localization": localization_json(&config) }))
}

/// `editor.project_packaging_edit { packaging, save? }`.
fn project_packaging_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = root_of(world)?;
    let mut state = ProjectSettingsState::default();
    state
        .load(&root)
        .map_err(|e| err("project_settings_read", e).rpc("check Regia.toml parses"))?;
    let mut page = PackagingPage::read(&state);
    let patch = params
        .get("packaging")
        .ok_or_else(|| err("packaging_invalid", "`packaging` is required").rpc(HINT))?;
    patch_packaging(&mut page, patch).map_err(|e| e.rpc(HINT))?;
    page.write(&mut state).map_err(|e| e.rpc(HINT))?;
    let save = params.get("save").and_then(Value::as_bool).unwrap_or(true);
    if save && state.is_dirty() {
        state
            .save(&root)
            .map_err(|e| err("project_settings_invalid", e).rpc("nothing was written"))?;
        world.insert_resource(crate::project_settings_ext::ProjectSettingsSaved);
    }
    Ok(json!({ "packaging": PackagingPage::read(&state).json(), "saved": save }))
}

/// `editor.script_wizard { open? }`: shows or hides the wizard and reports its state.
fn script_wizard_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut wizard = world.get_resource_or_insert_with(ScriptWizard::default);
    if let Some(open) = params.get("open").and_then(Value::as_bool) {
        wizard.open = open;
    }
    Ok(json!({
        "open": wizard.open,
        "name": wizard.name,
        "template": wizard.template.as_str(),
        "templates": ScriptTemplate::ALL.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
        "status": wizard.status,
    }))
}

/// The creators' agent methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.project_localization_edit",
                tool: "project_localization_edit",
                description: "Edit the source locale and the shipped locale list of assets/localization/localization.ron as the Project Settings Localization page does: all or nothing, BCP 47 tags checked, one undo step.",
                params: &[ParamSchema {
                    name: "edits",
                    kind: "array",
                    required: true,
                    description: "[{ op: \"source\" | \"add_locale\" | \"remove_locale\", locale }].",
                }],
                result: "{ localization: { path, source, locales } }",
                mutating: true,
                permission: Permission::EditScene,
            },
            project_localization_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.project_packaging_edit",
                tool: "project_packaging_edit",
                description: "Edit the packaging keys of Regia.toml as the Project Settings Packaging page does ([platforms.android] SDK levels, orientation, ABIs, permissions, signing references; [platforms.linux] formats), keeping every other key. Passwords must be env:VAR references. Saves by default.",
                params: &[
                    ParamSchema {
                        name: "packaging",
                        kind: "object",
                        required: true,
                        description: "{ android?: { min_sdk?, target_sdk?, orientation?, abis?, permissions?, keystore?, keystore_password?, key_alias?, key_password? }, linux?: { formats? } }.",
                    },
                    ParamSchema {
                        name: "save",
                        kind: "boolean",
                        required: false,
                        description: "Write Regia.toml (default true); false only validates.",
                    },
                ],
                result: "{ packaging: { android: {...}, linux: { formats } }, saved }",
                mutating: true,
                permission: Permission::EditScene,
            },
            project_packaging_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.script_wizard",
                tool: "script_wizard",
                description: "Show or hide the New script wizard (Tools ▸ New script…) and read its state. editor.create_script { name, template } creates the script the wizard's Create would.",
                params: &[ParamSchema {
                    name: "open",
                    kind: "boolean",
                    required: false,
                    description: "true shows the wizard, false hides it; absent only reads.",
                }],
                result: "{ open, name, template, templates, status }",
                mutating: true,
                permission: Permission::EditScene,
            },
            script_wizard_method as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world_with(root: &Path) -> World {
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.to_path_buf()),
        });
        world.insert_resource(UndoStack::new());
        world
    }

    #[test]
    fn localization_edits_check_tags_and_write_the_loader_shape() {
        let root = regia_test_utils::unique_temp_dir("creators-loc");
        let mut world = world_with(&root);
        let config = edit_localization(
            &mut world,
            &root,
            &[
                LocalizationEdit::AddLocale("fr".into()),
                LocalizationEdit::AddLocale("pt-BR".into()),
            ],
        )
        .expect("edit");
        assert_eq!(config.locales, ["fr", "pt-BR"]);
        let bad = edit_localization(
            &mut world,
            &root,
            &[LocalizationEdit::AddLocale("not a tag!".into())],
        );
        assert_eq!(bad.expect_err("bad").code, "localization_bad_locale");
        assert_eq!(load_localization(&root).expect("load").locales, ["fr", "pt-BR"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn packaging_keeps_other_keys_and_refuses_literal_passwords() {
        let root = regia_test_utils::unique_temp_dir("creators-pack");
        std::fs::write(
            root.join("Regia.toml"),
            "[project]\nname = \"G\"\nid = \"com.example.g\"\n\n[platforms.android]\nmin_sdk = 30\ncustom = 1\n",
        )
        .expect("manifest");
        let mut state = ProjectSettingsState::default();
        state.load(&root).expect("load");
        let mut page = PackagingPage::read(&state);
        assert_eq!(page.min_sdk, 30);
        patch_packaging(
            &mut page,
            &json!({ "android": { "orientation": "portrait", "keystore_password": "hunter2" } }),
        )
        .expect("patch");
        assert_eq!(
            page.write(&mut state).expect_err("literal").code,
            "packaging_secret_literal"
        );
        page.keystore_password = "env:KS_PASS".into();
        page.write(&mut state).expect("write");
        state.save(&root).expect("save");
        let text = std::fs::read_to_string(root.join("Regia.toml")).expect("read");
        assert!(text.contains("custom = 1"), "{text}");
        assert!(text.contains("orientation = \"portrait\""), "{text}");
        assert!(text.contains("env:KS_PASS"), "{text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_component_template_names_the_type_in_pascal_case() {
        let body = ScriptTemplate::Component.body("door_switch");
        assert!(body.contains("pub struct DoorSwitch {}"), "{body}");
        assert!(body.contains("#[reflect(Component, Default)]"), "{body}");
        assert_eq!(
            ScriptTemplate::parse("nope").expect_err("bad").code,
            "script_bad_template"
        );
    }
}
