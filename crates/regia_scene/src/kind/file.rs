// SPDX-License-Identifier: MPL-2.0
//! The `.kind.ron` format (format `kind`, v1; `docs/formats/kind-ron.md`, spec 101 § 2.3).
//!
//! A data kind's declaration: the same fields a code kind's [`KindMeta`](super::KindMeta) holds plus
//! the required component type paths, and optionally the code marker it describes. Entities carry a
//! data kind through [`KindTag`](super::KindTag).

use serde::{Deserialize, Serialize};

use super::{KindInfo, KindSource, MAX_KIND_DESCRIPTION, validate_kind_name};

/// The `.kind.ron` format version this build reads and writes.
pub const KIND_FORMAT_VERSION: u32 = 1;

/// File name suffix of a kind declaration (`crate.kind.ron`).
pub const KIND_FILE_SUFFIX: &str = ".kind.ron";

/// Errors reading a `.kind.ron`.
#[derive(Debug, thiserror::Error)]
pub enum KindFileError {
    /// The text is not valid RON for the format.
    #[error("RON: {0}")]
    Ron(#[from] ron::error::SpannedError),
    /// Serializing failed.
    #[error("RON: {0}")]
    RonWrite(#[from] ron::Error),
    /// Written by a newer engine.
    #[error(
        "kind format_version {found} is newer than this engine's {KIND_FORMAT_VERSION}; upgrade the engine"
    )]
    Newer {
        /// The version in the file.
        found: u32,
    },
    /// A field is invalid (name, color, description).
    #[error("invalid kind: {0}")]
    Invalid(String),
}

impl KindFileError {
    /// The agent error code of this error (`kind_version_newer`, `kind_parse`, `kind_bad_param`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Ron(_) | Self::RonWrite(_) => "kind_parse",
            Self::Newer { .. } => "kind_version_newer",
            Self::Invalid(_) => "kind_bad_param",
        }
    }
}

/// The document of a `.kind.ron` file.
///
/// ```ron
/// (
///     format_version: 1,
///     name: "Crate",
///     icon: "crate",
///     color: "#c08a3e",
///     description: "A breakable supply crate",
///     category: Some("Props"),
///     required: ["regia_scene::transform::Transform"],
///     marker: None,
/// )
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindFile {
    /// Format version; newer is refused.
    pub format_version: u32,
    /// Unique label, `[A-Za-z0-9_]`, 1-64 bytes.
    pub name: String,
    /// Icon name (`kind` when empty).
    #[serde(default)]
    pub icon: String,
    /// sRGB `#rrggbb`.
    pub color: String,
    /// One line, at most 120 characters.
    #[serde(default)]
    pub description: String,
    /// Display group.
    #[serde(default)]
    pub category: Option<String>,
    /// Component type paths the kind's entities must carry.
    #[serde(default)]
    pub required: Vec<String>,
    /// The code marker type path this declaration describes, for tools that do not link the game.
    #[serde(default)]
    pub marker: Option<String>,
}

/// Parses `#rrggbb` (or `rrggbb`).
///
/// # Errors
///
/// The text is not six hex digits.
pub fn parse_color(text: &str) -> Result<[u8; 3], String> {
    let hex = text.strip_prefix('#').unwrap_or(text);
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("color `{text}` is not #rrggbb"));
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| e.to_string());
    Ok([byte(0)?, byte(2)?, byte(4)?])
}

impl KindFile {
    /// Parses a `.kind.ron`, refusing a newer version (peeked first, so a newer file is refused
    /// even when its body no longer parses) and validating every field.
    ///
    /// # Errors
    ///
    /// See [`KindFileError`].
    pub fn from_ron_str(text: &str) -> Result<Self, KindFileError> {
        if let Ok(found) = regia_core::version::peek_ron_version(text)
            && found > KIND_FORMAT_VERSION
        {
            return Err(KindFileError::Newer { found });
        }
        let file: Self = ron::from_str(text)?;
        if file.format_version == 0 {
            return Err(KindFileError::Invalid(
                "format_version 0 does not exist; use 1".into(),
            ));
        }
        file.validate()?;
        Ok(file)
    }

    /// Checks name, color, description and the required list.
    ///
    /// # Errors
    ///
    /// [`KindFileError::Invalid`] with the first problem.
    pub fn validate(&self) -> Result<(), KindFileError> {
        validate_kind_name(&self.name).map_err(KindFileError::Invalid)?;
        parse_color(&self.color).map_err(KindFileError::Invalid)?;
        if self.description.chars().count() > MAX_KIND_DESCRIPTION {
            return Err(KindFileError::Invalid(format!(
                "description is longer than {MAX_KIND_DESCRIPTION} characters"
            )));
        }
        if self.description.contains('\n') {
            return Err(KindFileError::Invalid("description must be one line".into()));
        }
        if let Some(empty) = self.required.iter().find(|r| r.trim().is_empty()) {
            return Err(KindFileError::Invalid(format!(
                "required has an empty entry `{empty}`"
            )));
        }
        Ok(())
    }

    /// The canonical RON text.
    ///
    /// # Errors
    ///
    /// Serialization failed.
    pub fn to_ron_string(&self) -> Result<String, KindFileError> {
        let mut text = ron::ser::to_string_pretty(self, regia_ecs::serialize::canonical_pretty_config())?;
        text.push('\n');
        Ok(text)
    }

    /// The registry entry for this declaration, read from `path` (project-relative).
    ///
    /// # Errors
    ///
    /// The color does not parse (a validated file always does).
    pub fn to_info(&self, path: &str) -> Result<KindInfo, KindFileError> {
        let mut required = self.required.clone();
        required.sort();
        required.dedup();
        Ok(KindInfo {
            name: self.name.clone(),
            icon: if self.icon.is_empty() {
                "kind".to_string()
            } else {
                self.icon.clone()
            },
            color: parse_color(&self.color).map_err(KindFileError::Invalid)?,
            description: self.description.clone(),
            category: self.category.clone(),
            required,
            source: KindSource::Data {
                path: path.to_string(),
                marker: self.marker.clone(),
            },
        })
    }

    /// A declaration of a registered kind (for writing a code kind's `.kind.ron`).
    #[must_use]
    pub fn from_info(info: &KindInfo) -> Self {
        Self {
            format_version: KIND_FORMAT_VERSION,
            name: info.name.clone(),
            icon: info.icon.clone(),
            color: info.color_hex(),
            description: info.description.clone(),
            category: info.category.clone(),
            required: info.required.clone(),
            marker: info.source.marker().map(str::to_string),
        }
    }
}

/// The serde field-list fingerprint of the `.kind.ron` format (hashed by `cargo xtask check-formats`).
#[must_use]
pub fn kind_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[regia_core::schema::serde_field_names::<KindFile>()])
}

/// The `from` versions of the registered migration steps (none at v1).
#[must_use]
pub fn kind_migration_sources() -> Vec<u32> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CRATE: &str = r##"(
        format_version: 1,
        name: "Crate",
        icon: "crate",
        color: "#c08a3e",
        description: "A breakable supply crate",
        category: Some("Props"),
        required: ["regia_scene::transform::Transform", "regia_scene::transform::Transform"],
    )"##;

    #[test]
    fn reads_and_round_trips() {
        let file = KindFile::from_ron_str(CRATE).expect("parse");
        assert_eq!(file.name, "Crate");
        assert_eq!(file.marker, None);
        let info = file.to_info("assets/kinds/crate.kind.ron").expect("info");
        assert_eq!(info.color, [0xc0, 0x8a, 0x3e]);
        assert_eq!(
            info.required,
            ["regia_scene::transform::Transform"],
            "sorted, deduplicated"
        );
        assert_eq!(info.category_name(), "Props");
        let text = file.to_ron_string().expect("write");
        assert_eq!(KindFile::from_ron_str(&text).expect("re-read"), file);
    }

    #[test]
    fn refuses_newer_versions_and_bad_fields() {
        let newer = CRATE.replace("format_version: 1", "format_version: 2");
        let err = KindFile::from_ron_str(&newer).expect_err("newer");
        assert_eq!(err.code(), "kind_version_newer");
        let zero = CRATE.replace("format_version: 1", "format_version: 0");
        assert_eq!(
            KindFile::from_ron_str(&zero).expect_err("zero").code(),
            "kind_bad_param"
        );
        let name = CRATE.replace("\"Crate\"", "\"Big crate\"");
        assert_eq!(
            KindFile::from_ron_str(&name).expect_err("name").code(),
            "kind_bad_param"
        );
        let color = CRATE.replace("#c08a3e", "orange");
        assert_eq!(
            KindFile::from_ron_str(&color).expect_err("color").code(),
            "kind_bad_param"
        );
        assert_eq!(KindFile::from_ron_str("(").expect_err("ron").code(), "kind_parse");
    }

    #[test]
    fn empty_icon_reads_as_the_generic_icon() {
        let file = KindFile::from_ron_str(&CRATE.replace("icon: \"crate\",", "")).expect("parse");
        assert_eq!(file.to_info("k.kind.ron").expect("info").icon, "kind");
    }

    #[test]
    fn fingerprint_lists_every_field() {
        let fingerprint = kind_schema_fingerprint();
        for field in [
            "format_version",
            "name",
            "icon",
            "color",
            "description",
            "category",
            "required",
            "marker",
        ] {
            assert!(fingerprint.contains(field), "{field} in {fingerprint}");
        }
    }
}
