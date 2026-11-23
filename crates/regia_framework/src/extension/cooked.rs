// SPDX-License-Identifier: MPL-2.0
//! The cooked extension record (`specs/57` § 4.4): `extensions.ron`, inside the paks and next to
//! them.
//!
//! `regia cook` writes the resolved set (`format_version`, ordered ids, versions, manifest
//! hashes). A player compares it with the set it resolved and reports
//! `extension_set_mismatch` when the content and the binary disagree about extensions. The copy
//! in the pak (manifest path [`COOKED_ASSET_PATH`]) is the one a packaged game reads, on every
//! platform; the file next to the paks serves a development run before anything mounts.

use serde::{Deserialize, Serialize};

use super::resolver::{DiscoveredManifest, ResolvedSet};

/// The file name, in `build/paks/<platform>/`.
pub const COOKED_FILE: &str = "extensions.ron";
/// The record's path in the cooked manifest. `regia` is a reserved id, so no extension's content
/// can land on it.
pub const COOKED_ASSET_PATH: &str = "ext://regia/extensions.ron";
/// The version this build writes and reads.
pub const COOKED_FORMAT_VERSION: u32 = 1;

/// One extension of a cooked set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CookedExtension {
    /// Extension id.
    pub id: String,
    /// Its version.
    pub version: String,
    /// Hex hash of its manifest bytes.
    pub manifest_hash: String,
}

/// The resolved set a cook saw, in load order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CookedExtensions {
    /// [`COOKED_FORMAT_VERSION`].
    pub format_version: u32,
    /// The resolver's hash of the set.
    pub resolved_hash: String,
    /// Extensions in load order.
    pub extensions: Vec<CookedExtension>,
}

/// Why a record could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CookedError {
    /// The file is not valid RON of this shape.
    #[error("manifest_invalid: {COOKED_FILE}: {0}")]
    Invalid(String),
    /// A newer engine wrote it.
    #[error(
        "manifest_too_new: {COOKED_FILE} is format_version {0}; this build reads {COOKED_FORMAT_VERSION}"
    )]
    TooNew(u32),
}

/// The serde field-list fingerprint of `extensions.ron` (hashed by `cargo xtask check-formats`).
#[must_use]
pub fn cooked_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<CookedExtensions>(),
        regia_core::schema::serde_field_names::<CookedExtension>(),
    ])
}

/// `extensions.ron` is derived data with no migration steps yet (v1): cook again.
#[must_use]
pub fn cooked_migration_sources() -> Vec<u32> {
    Vec::new()
}

impl CookedExtensions {
    /// The record of `set`, with hashes from `discovered`.
    #[must_use]
    pub fn of(set: &ResolvedSet, discovered: &[DiscoveredManifest]) -> Self {
        let hash_of = |id: &str| {
            discovered
                .iter()
                .find(|d| d.manifest.id == id)
                .map(|d| d.hash.clone())
                .unwrap_or_default()
        };
        Self {
            format_version: COOKED_FORMAT_VERSION,
            resolved_hash: set.resolved_hash.clone(),
            extensions: set
                .order
                .iter()
                .map(|e| CookedExtension {
                    id: e.id.clone(),
                    version: e.version.to_string(),
                    manifest_hash: hash_of(&e.id),
                })
                .collect(),
        }
    }

    /// The RON text.
    ///
    /// # Errors
    ///
    /// Serialization failed.
    pub fn to_ron(&self) -> Result<String, CookedError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| CookedError::Invalid(e.to_string()))
    }

    /// Reads a record.
    ///
    /// # Errors
    ///
    /// [`CookedError`].
    pub fn parse(text: &str) -> Result<Self, CookedError> {
        // The version first, so a newer shape reads as too new and not as malformed.
        if let Ok(probe) = ron::from_str::<VersionOnly>(text)
            && probe.format_version > COOKED_FORMAT_VERSION
        {
            return Err(CookedError::TooNew(probe.format_version));
        }
        let record: Self = ron::from_str(text).map_err(|e| CookedError::Invalid(e.to_string()))?;
        // The same shape at a newer version is just as unreadable.
        if record.format_version > COOKED_FORMAT_VERSION {
            return Err(CookedError::TooNew(record.format_version));
        }
        Ok(record)
    }

    /// `None` when `self` and `set` agree on ids, order, versions and manifests, else what differs.
    #[must_use]
    pub fn mismatch(&self, set: &ResolvedSet, discovered: &[DiscoveredManifest]) -> Option<String> {
        let now = Self::of(set, discovered);
        if now.extensions == self.extensions {
            return None;
        }
        let names = |list: &[CookedExtension]| {
            list.iter()
                .map(|e| format!("{} {}", e.id, e.version))
                .collect::<Vec<_>>()
                .join(", ")
        };
        Some(format!(
            "the content was cooked with [{}]; this binary resolves [{}]",
            names(&self.extensions),
            names(&now.extensions)
        ))
    }
}

/// Reads only the version; every other field of a record is skipped whatever its shape.
#[derive(Deserialize)]
struct VersionOnly {
    format_version: u32,
}
