// SPDX-License-Identifier: MPL-2.0
//! `.bike.ron` v1 (ADR-8630): a named [`BikeConfig`] an artist ships. See `docs/formats/bike-ron.md`.
//!
//! ```ron
//! (
//!     format_version: 1,
//!     name: "city",
//!     config: (wheelbase: 1.4, lean_response: 5.0),
//!     notes: Some("a light commuter: quick to lean"),
//! )
//! ```
//!
//! Fields are named, never positional. Every `config` field is optional: a missing one takes
//! [`BikeConfig::default`]'s value, and a missing `config` block is the default configuration. An
//! unknown field name anywhere is `bike_parse` and the message names it. The loader peeks
//! `format_version` before the typed parse, so a newer file is `bike_version` naming its version
//! even when its body is a shape this build cannot read. RON `implicit_some` is on, so
//! `notes: "text"` and `notes: Some("text")` are the same.
//!
//! [`BikeAsset::to_ron`] writes the canonical text: every field, four-space indent, LF line ends
//! and a final LF.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{BikeConfig, BikeError};

/// Current `.bike.ron` format version.
pub const BIKE_FORMAT_VERSION: u32 = 1;

/// Versions this build reads, and the version a file of that generation is written back as.
/// v1 reads as v1 (there is no older layout).
pub const BIKE_VERSION_TABLE: &[(u32, u32)] = &[(1, 1)];

/// A `.bike.ron`: a display name, the bike's configuration and an optional note.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BikeAsset {
    /// Must be [`BIKE_FORMAT_VERSION`].
    pub format_version: u32,
    /// A display name. Empty or only whitespace is `bike_bad_name`.
    pub name: String,
    /// The bike. A field the file leaves out is [`BikeConfig::default`]'s.
    #[serde(default)]
    pub config: BikeConfig,
    /// Free text a tool may show. The step does not read it.
    #[serde(default)]
    pub notes: Option<String>,
}

impl Default for BikeAsset {
    /// The default configuration, named `bike`, at the current version.
    fn default() -> Self {
        Self {
            format_version: BIKE_FORMAT_VERSION,
            name: "bike".into(),
            config: BikeConfig::default(),
            notes: None,
        }
    }
}

/// Why a `.bike.ron` did not load or validate. [`BikeAssetError::code`] is the protocol code.
#[derive(Debug, Clone, PartialEq)]
pub enum BikeAssetError {
    /// The text is not a named `.bike.ron`: bad RON, a positional tuple, an unknown or misspelt
    /// field (named in the message), or no `format_version` (`bike_parse`).
    Parse(String),
    /// `format_version` is not [`BIKE_FORMAT_VERSION`] (`bike_version`).
    Version {
        /// The version the file asked for.
        found: u32,
        /// The version this build reads.
        expected: u32,
    },
    /// `name` is empty or only whitespace (`bike_bad_name`).
    BadName,
    /// A configuration field is not finite or is outside its range: [`BikeError::BadInput`]'s
    /// code, `bike_bad_input`, which [`super::bike_step`] answers for the same configuration.
    Field {
        /// The field name (`wheelbase`, `max_lean`, ...).
        field: &'static str,
        /// The value it holds.
        value: f32,
        /// The valid range, `[min, max]` inclusive ([`BikeConfig::FIELD_RANGES`]).
        range: [f32; 2],
    },
}

impl BikeAssetError {
    /// The error code: lowercase ascii, digits and `_`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse(_) => "bike_parse",
            Self::Version { .. } => "bike_version",
            Self::BadName => "bike_bad_name",
            Self::Field { .. } => BikeError::BadInput.code(),
        }
    }

    /// The field the error names, when it names one (`name`, `format_version` or a config field).
    #[must_use]
    pub fn field_name(&self) -> Option<&str> {
        match self {
            Self::Field { field, .. } => Some(field),
            Self::BadName => Some("name"),
            Self::Version { .. } => Some("format_version"),
            Self::Parse(_) => None,
        }
    }
}

impl fmt::Display for BikeAssetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(message) => write!(f, "bike_parse: {message}"),
            Self::Version { found, expected } => write!(
                f,
                "bike_version: .bike.ron version {found} (this build reads {expected})"
            ),
            Self::BadName => write!(f, "bike_bad_name: bike name is empty"),
            Self::Field { field, value, range } => {
                let [min, max] = range;
                write!(
                    f,
                    "bike_bad_input: bike field `{field}` = {value} (valid {min} to {max})"
                )
            }
        }
    }
}

impl std::error::Error for BikeAssetError {}

impl BikeAsset {
    /// Parses and validates a `.bike.ron` text.
    ///
    /// # Errors
    /// [`BikeAssetError`]: `bike_parse`, `bike_version` (checked before the body is parsed),
    /// `bike_bad_name` or `bike_bad_input` naming the field.
    pub fn from_ron(text: &str) -> Result<Self, BikeAssetError> {
        let found = regia_core::version::peek_ron_version(text).map_err(BikeAssetError::Parse)?;
        if found != BIKE_FORMAT_VERSION {
            return Err(BikeAssetError::Version {
                found,
                expected: BIKE_FORMAT_VERSION,
            });
        }
        let asset: Self = ron::Options::default()
            .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
            .from_str(text)
            .map_err(|err| BikeAssetError::Parse(err.to_string()))?;
        asset.validate()?;
        Ok(asset)
    }

    /// Checks the version, the name and every configuration field.
    ///
    /// # Errors
    /// The same codes as [`BikeAsset::from_ron`].
    pub fn validate(&self) -> Result<(), BikeAssetError> {
        if self.format_version != BIKE_FORMAT_VERSION {
            return Err(BikeAssetError::Version {
                found: self.format_version,
                expected: BIKE_FORMAT_VERSION,
            });
        }
        if self.name.trim().is_empty() {
            return Err(BikeAssetError::BadName);
        }
        match self.config.first_bad_field() {
            Some((field, value, range)) => Err(BikeAssetError::Field { field, value, range }),
            None => Ok(()),
        }
    }

    /// The canonical text: pretty RON with every field, LF line ends and a final LF.
    ///
    /// # Errors
    /// [`BikeAssetError::Parse`] when RON cannot write the value (never for a valid one).
    pub fn to_ron(&self) -> Result<String, BikeAssetError> {
        let mut text = ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|err| BikeAssetError::Parse(err.to_string()))?;
        if !text.ends_with('\n') {
            text.push('\n');
        }
        Ok(text)
    }

    /// The configuration, consuming the asset.
    #[must_use]
    pub fn into_config(self) -> BikeConfig {
        self.config
    }
}

/// Reads a v1 file and writes it back in canonical form. Any other version is `bike_version`.
///
/// # Errors
/// The same codes as [`BikeAsset::from_ron`].
pub fn migrate_bike(text: &str) -> Result<String, BikeAssetError> {
    BikeAsset::from_ron(text)?.to_ron()
}

/// Versions a migrator accepts below the current one (none at v1).
#[must_use]
pub fn bike_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// Serde field-list fingerprint of `.bike.ron` (for `cargo xtask check-formats`).
#[must_use]
pub fn bike_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    join_field_lists(&[
        serde_field_names::<BikeAsset>(),
        serde_field_names::<BikeConfig>(),
    ])
}

#[cfg(test)]
#[path = "asset_tests.rs"]
mod tests;
