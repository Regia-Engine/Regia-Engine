// SPDX-License-Identifier: MPL-2.0
//! `assets/localization/localization.ron` (format `localization` v1, `docs/specs/content-data.md`
//! § 1.2, ADR-1466): the source locale, the locale list, extra fallback edges and the gather
//! roots. The runtime reads `source` and `fallbacks`; the tools read all of it. A project without
//! the file gets [`LocalizationConfig::default`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The current `localization.ron` version.
pub const LOCALIZATION_CONFIG_VERSION: u32 = 1;

/// Where the file lives, relative to the project root.
pub const LOCALIZATION_CONFIG_PATH: &str = "assets/localization/localization.ron";

/// What `regia loc gather` reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GatherConfig {
    /// Directories (relative to the project root) walked for `.ron` and `.rs` files.
    pub roots: Vec<String>,
    /// Path prefixes (relative to the project root) skipped by the walk.
    pub exclude: Vec<String>,
}

impl Default for GatherConfig {
    fn default() -> Self {
        Self {
            roots: vec!["assets".to_owned(), "src".to_owned()],
            exclude: vec!["assets/localization".to_owned()],
        }
    }
}

/// `localization.ron`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalizationConfig {
    /// Must be [`LOCALIZATION_CONFIG_VERSION`].
    pub format_version: u32,
    /// The locale keys are authored in, and the last fallback of every chain.
    pub source: String,
    /// The locales the project ships (the gather tool writes a file for each). Empty: every
    /// locale directory found.
    pub locales: Vec<String>,
    /// Extra fallback edges, tried before a locale's parents (`"pt-BR": ["pt-PT"]`).
    pub fallbacks: BTreeMap<String, Vec<String>>,
    /// Gather roots.
    pub gather: GatherConfig,
}

impl Default for LocalizationConfig {
    fn default() -> Self {
        Self {
            format_version: LOCALIZATION_CONFIG_VERSION,
            source: "en".to_owned(),
            locales: Vec::new(),
            fallbacks: BTreeMap::new(),
            gather: GatherConfig::default(),
        }
    }
}

/// Errors reading `localization.ron`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LocalizationConfigError {
    /// The file is not valid RON for the shape.
    #[error("localization.ron does not parse: {0}")]
    Parse(String),
    /// A version this build does not know.
    #[error("localization.ron has format_version {found}; this build reads up to {supported}")]
    Version {
        /// The file's version.
        found: u32,
        /// The newest version this build reads.
        supported: u32,
    },
    /// A locale tag that is not a valid BCP 47 language tag.
    #[error("localization.ron names a bad locale `{0}`")]
    BadLocale(String),
}

impl LocalizationConfig {
    /// Parses the file's text.
    ///
    /// # Errors
    ///
    /// The text does not parse, the version is newer than this build, or a locale tag is invalid.
    pub fn from_ron(text: &str) -> Result<Self, LocalizationConfigError> {
        let config: Self = ron::from_str(text).map_err(|e| LocalizationConfigError::Parse(e.to_string()))?;
        if config.format_version > LOCALIZATION_CONFIG_VERSION || config.format_version == 0 {
            return Err(LocalizationConfigError::Version {
                found: config.format_version,
                supported: LOCALIZATION_CONFIG_VERSION,
            });
        }
        let tags = std::iter::once(&config.source)
            .chain(&config.locales)
            .chain(config.fallbacks.keys())
            .chain(config.fallbacks.values().flatten());
        for tag in tags {
            if tag.parse::<unic_langid::LanguageIdentifier>().is_err() {
                return Err(LocalizationConfigError::BadLocale(tag.clone()));
            }
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_fill_missing_fields() {
        let c = LocalizationConfig::from_ron("(format_version: 1, fallbacks: { \"pt-BR\": [\"pt-PT\"] })")
            .expect("parse");
        assert_eq!(c.source, "en");
        assert_eq!(c.fallbacks["pt-BR"], vec!["pt-PT".to_owned()]);
        assert_eq!(c.gather, GatherConfig::default());
    }

    #[test]
    fn rejects_new_versions_and_bad_tags() {
        assert!(matches!(
            LocalizationConfig::from_ron("(format_version: 2)"),
            Err(LocalizationConfigError::Version { found: 2, .. })
        ));
        assert!(matches!(
            LocalizationConfig::from_ron("(format_version: 1, source: \"not a tag!\")"),
            Err(LocalizationConfigError::BadLocale(_))
        ));
        assert!(matches!(
            LocalizationConfig::from_ron("(format_version: 1, nope: 3)"),
            Err(LocalizationConfigError::Parse(_))
        ));
    }
}
