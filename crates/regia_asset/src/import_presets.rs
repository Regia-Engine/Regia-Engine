// SPDX-License-Identifier: MPL-2.0
//! Named import presets (`config/import_presets.ron`, format `import-presets` v1;
//! `docs/formats/import-presets-ron.md`, ADR-7904).
//!
//! A preset is a named importer settings object for one importer kind (`texture`, `gltf`,
//! `obj`, `fbx`, `audio`): the same RON settings text a source's `.meta` carries. The editor's
//! Import settings panel saves the form as a preset and applies a preset to the sources it edits;
//! the import dialog applies one to every file of a multi-file import. The file is shared by the
//! project (it is committed next to `config/import_rules.ron`); a preset never applies by itself
//! (that is what import rules do).
//!
//! ```ron
//! (
//!     format_version: 1,
//!     presets: [
//!         (name: "UI sprite", importer: "texture", settings: "(usage: Some(ui), compress: Some(false))"),
//!         (name: "Character", importer: "gltf", settings: "(skin_influences: 8, scale: 0.01)"),
//!     ],
//! )
//! ```

use serde::{Deserialize, Serialize};

/// Current `import-presets` format version.
pub const IMPORT_PRESETS_FORMAT_VERSION: u32 = 1;

/// Project-relative path of the presets file.
pub const IMPORT_PRESETS_PATH: &str = "config/import_presets.ron";

/// Longest preset name accepted, in bytes.
pub const MAX_PRESET_NAME: usize = 128;

/// The importer kinds a preset may name.
pub const PRESET_IMPORTERS: &[&str] = &["texture", "gltf", "obj", "fbx", "audio"];

/// One preset.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportPreset {
    /// The name (unique per importer, compared ignoring case).
    pub name: String,
    /// The importer kind ([`PRESET_IMPORTERS`]).
    pub importer: String,
    /// The importer settings object, as RON text (`(max_size: Some(512))`); `()` for none.
    pub settings: String,
}

/// The presets file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportPresetsFile {
    /// Must be [`IMPORT_PRESETS_FORMAT_VERSION`].
    pub format_version: u32,
    /// Presets in display order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presets: Vec<ImportPreset>,
}

impl Default for ImportPresetsFile {
    fn default() -> Self {
        Self {
            format_version: IMPORT_PRESETS_FORMAT_VERSION,
            presets: Vec::new(),
        }
    }
}

/// Why a presets file (or a preset) could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresetsError(pub String);

impl std::fmt::Display for PresetsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PresetsError {}

/// Why `name` is not a valid preset name (`None` when it is): not blank, at most
/// [`MAX_PRESET_NAME`] bytes, no control characters.
#[must_use]
pub fn preset_name_problem(name: &str) -> Option<&'static str> {
    if name.trim().is_empty() {
        Some("is empty")
    } else if name.len() > MAX_PRESET_NAME {
        Some("is too long")
    } else if name.chars().any(char::is_control) {
        Some("contains a control character")
    } else {
        None
    }
}

fn same_name(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

impl ImportPresetsFile {
    /// Parses a presets file: the version must be current, every importer known, names valid
    /// and unique per importer (ignoring case), and every settings text a RON settings object.
    ///
    /// # Errors
    ///
    /// [`PresetsError`] naming the problem.
    pub fn from_ron(text: &str) -> Result<Self, PresetsError> {
        let version = regia_core::version::peek_ron_version(text).map_err(|e| PresetsError(e.to_string()))?;
        if version != IMPORT_PRESETS_FORMAT_VERSION {
            return Err(PresetsError(format!(
                "format_version {version} is not supported (expected {IMPORT_PRESETS_FORMAT_VERSION})"
            )));
        }
        let file: Self = ron::from_str(text).map_err(|e| PresetsError(e.to_string()))?;
        file.validate()?;
        Ok(file)
    }

    /// Checks importers, names and settings text (see [`Self::from_ron`]).
    ///
    /// # Errors
    ///
    /// [`PresetsError`] naming the first problem.
    pub fn validate(&self) -> Result<(), PresetsError> {
        for (i, preset) in self.presets.iter().enumerate() {
            if !PRESET_IMPORTERS.contains(&preset.importer.as_str()) {
                return Err(PresetsError(format!(
                    "preset {i}: importer {:?} is not one of {}",
                    preset.importer,
                    PRESET_IMPORTERS.join(", ")
                )));
            }
            if let Some(problem) = preset_name_problem(&preset.name) {
                return Err(PresetsError(format!(
                    "preset {i}: the name {:?} {problem}",
                    preset.name
                )));
            }
            if self.presets[..i]
                .iter()
                .any(|p| p.importer == preset.importer && same_name(&p.name, &preset.name))
            {
                return Err(PresetsError(format!(
                    "preset {i}: the {} preset name {:?} is used twice",
                    preset.importer, preset.name
                )));
            }
            crate::import_rules::SettingsObject::parse(&preset.settings).map_err(|e| {
                PresetsError(format!(
                    "preset {:?}: the settings are not a RON settings object: {}",
                    preset.name, e.message
                ))
            })?;
        }
        Ok(())
    }

    /// The RON text of this file (pretty, deterministic).
    ///
    /// # Errors
    ///
    /// [`PresetsError`] when serialization fails.
    pub fn to_ron_string(&self) -> Result<String, PresetsError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty().struct_names(false))
            .map_err(|e| PresetsError(e.to_string()))
    }

    /// The `importer` preset named `name` (ignoring case).
    #[must_use]
    pub fn find(&self, importer: &str, name: &str) -> Option<&ImportPreset> {
        self.presets
            .iter()
            .find(|p| p.importer == importer && same_name(&p.name, name))
    }

    /// Adds `preset`, or replaces the settings of the preset with its importer and name (keeping
    /// its place and its stored name). Answers whether one was replaced.
    ///
    /// # Errors
    ///
    /// [`PresetsError`] when the preset is not valid ([`Self::validate`]'s rules); the file is
    /// unchanged then.
    pub fn upsert(&mut self, preset: ImportPreset) -> Result<bool, PresetsError> {
        let mut next = self.clone();
        let replaced = match next
            .presets
            .iter_mut()
            .find(|p| p.importer == preset.importer && same_name(&p.name, &preset.name))
        {
            Some(existing) => {
                existing.settings = preset.settings;
                true
            }
            None => {
                next.presets.push(preset);
                false
            }
        };
        next.validate()?;
        *self = next;
        Ok(replaced)
    }

    /// Removes the `importer` preset named `name` (ignoring case). Answers whether one was there.
    pub fn remove(&mut self, importer: &str, name: &str) -> bool {
        let before = self.presets.len();
        self.presets
            .retain(|p| !(p.importer == importer && same_name(&p.name, name)));
        self.presets.len() != before
    }
}

/// The serde shape of `import_presets.ron` for `cargo xtask check-formats`
/// (`docs/formats/import-presets-ron.md`, ADR-7904).
#[must_use]
pub fn import_presets_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<ImportPresetsFile>(),
        regia_core::schema::serde_field_names::<ImportPreset>(),
    ])
}

/// `import_presets.ron` is at its first version: no migration steps.
#[must_use]
pub fn import_presets_migration_sources() -> Vec<u32> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_documented_example_round_trips() {
        let text = r#"(
            format_version: 1,
            presets: [
                (name: "UI sprite", importer: "texture", settings: "(usage: Some(ui), compress: Some(false))"),
                (name: "Character", importer: "gltf", settings: "(skin_influences: 8, scale: 0.01)"),
            ],
        )"#;
        let file = ImportPresetsFile::from_ron(text).expect("parse");
        assert_eq!(file.presets.len(), 2);
        let again = ImportPresetsFile::from_ron(&file.to_ron_string().expect("ron")).expect("reparse");
        assert_eq!(again, file);
        assert_eq!(
            file.find("texture", "ui SPRITE").map(|p| p.settings.as_str()),
            Some("(usage: Some(ui), compress: Some(false))")
        );
        assert!(file.find("gltf", "UI sprite").is_none(), "names are per importer");
    }

    #[test]
    fn bad_files_are_refused_with_the_problem() {
        let refused = |text: &str| ImportPresetsFile::from_ron(text).expect_err(text).0;
        assert!(refused("(format_version: 2)").contains("not supported"));
        assert!(
            refused(r#"(format_version: 1, presets: [(name: "a", importer: "mesh", settings: "()")])"#)
                .contains("importer")
        );
        assert!(
            refused(r#"(format_version: 1, presets: [(name: " ", importer: "gltf", settings: "()")])"#)
                .contains("is empty")
        );
        assert!(
            refused(
                r#"(format_version: 1, presets: [(name: "A", importer: "gltf", settings: "()"), (name: "a", importer: "gltf", settings: "()")])"#
            )
            .contains("used twice")
        );
        assert!(
            refused(r#"(format_version: 1, presets: [(name: "a", importer: "gltf", settings: "(scale: ")])"#)
                .contains("settings")
        );
        assert!(refused(r#"(format_version: 1, extra: 1)"#).contains("extra"));
        // The same name under two importers is fine.
        ImportPresetsFile::from_ron(
            r#"(format_version: 1, presets: [(name: "A", importer: "gltf", settings: "()"), (name: "A", importer: "fbx", settings: "()")])"#,
        )
        .expect("per-importer names");
    }

    #[test]
    fn upsert_replaces_by_name_and_remove_drops() {
        let mut file = ImportPresetsFile::default();
        let preset = |name: &str, settings: &str| ImportPreset {
            name: name.into(),
            importer: "texture".into(),
            settings: settings.into(),
        };
        assert_eq!(file.upsert(preset("Small", "(max_size: Some(64))")), Ok(false));
        assert_eq!(file.upsert(preset("small", "(max_size: Some(32))")), Ok(true));
        assert_eq!(file.presets.len(), 1);
        assert_eq!(file.presets[0].name, "Small", "the stored name is kept");
        assert_eq!(file.presets[0].settings, "(max_size: Some(32))");
        assert!(file.upsert(preset("Bad", "(max_size: ")).is_err());
        assert_eq!(
            file.presets.len(),
            1,
            "a refused upsert leaves the file as it was"
        );
        assert!(file.remove("texture", "SMALL"));
        assert!(!file.remove("texture", "small"));
        assert!(file.presets.is_empty());
    }
}
