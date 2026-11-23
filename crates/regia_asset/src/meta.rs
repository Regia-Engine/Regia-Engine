// SPDX-License-Identifier: MPL-2.0
//! `.meta` sidecar format v2 (`docs/specs/12-assets.md` § 1, `docs/specs/23` § 5,
//! `docs/formats/meta.md`). v2 (ADR-7819) adds the importer setting `lossless_clips`; a v1 file
//! migrates with it off.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_core::version::{self, VersionStep};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::id::AssetId;

/// Current `.meta` format version (v1 frozen at M4; v2 adds the `lossless_clips` importer
/// setting, ADR-7819).
pub const META_FORMAT_VERSION: u32 = 2;

/// The importer settings key (`.meta` v2, ADR-7819) that keeps every animation clip a source
/// produces lossless at cook: `settings: "(lossless_clips: true)"`. Read by
/// [`clip_cook_lossless`]; absent means off.
pub const LOSSLESS_CLIPS_KEY: &str = "lossless_clips";

/// The importer settings keys `.meta` v2 added on top of v1's (ADR-7819). Hashed into
/// [`meta_schema_fingerprint`], so `cargo xtask check-formats` sees a change to the list.
pub const META_V2_SETTINGS_KEYS: &[&str] = &[LOSSLESS_CLIPS_KEY];

/// Whether `settings` (a `.meta` `settings` string, or the effective settings with the project's
/// import rules merged in) keeps the source's animation clips lossless at cook (ADR-7819): the
/// [`LOSSLESS_CLIPS_KEY`] field. Blank settings, settings without the field and settings that are
/// not a RON object (the importer reports those) are `false`.
///
/// # Errors
///
/// A message when the field is there but is not `true` or `false`.
pub fn clip_cook_lossless(settings: Option<&str>) -> Result<bool, String> {
    let Some(text) = settings.filter(|s| !s.trim().is_empty()) else {
        return Ok(false);
    };
    let Ok(object) = crate::import_rules::SettingsObject::parse(text) else {
        return Ok(false);
    };
    match object.get(LOSSLESS_CLIPS_KEY).map(str::trim) {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(other) => Err(format!(
            "{LOSSLESS_CLIPS_KEY} must be true or false, got `{other}`"
        )),
    }
}

/// Provenance block on a source asset (`specs/23` § 5).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// Human-readable origin string (e.g. path, generator name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// How the asset was produced.
    #[serde(default)]
    pub tool: ProvenanceTool,
    /// Generator name when `tool` is [`ProvenanceTool::Generator`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generator: Option<String>,
    /// Generator seed when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    /// Agent client name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// Agent session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Model name reported by the client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// UTC timestamp (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_utc: Option<String>,
    /// Review state.
    #[serde(default)]
    pub reviewer: ReviewerState,
    /// Project-defined extension fields (`config/provenance.ron`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, String>,
}

/// Who created or last touched the asset.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceTool {
    /// Authored by hand in the editor or on disk.
    Hand,
    /// Created through the agent protocol.
    Agent,
    /// Produced by a declared generator.
    Generator,
    /// Imported from an external tool.
    Import,
    /// Unknown / legacy.
    #[default]
    Unknown,
}

/// Review workflow state for shipping cooks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewerState {
    /// Waiting for owner review.
    #[default]
    Pending,
    /// Approved by the project owner.
    Owner,
    /// Approved by a named reviewer.
    Named(String),
}

/// One sub-asset exported from a compound source (glTF mesh, material, clip, ...).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubAssetEntry {
    /// Stable path inside the source (`mesh/Body`, `material/Default`, ...).
    pub path: String,
    /// Deterministic id stored in the meta file.
    pub id: AssetId,
    /// Cooked type tag string (reflect path).
    pub type_path: String,
}

/// Regia Engine `.meta` sidecar v2.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetaSidecar {
    /// Format version; [`META_FORMAT_VERSION`] once loaded (older files migrate on load).
    pub format_version: u32,
    /// Primary asset id for this source file.
    pub id: AssetId,
    /// Importer id (`gltf`, `png`, `material_ron`, ...).
    pub importer: String,
    /// Importer settings as RON-compatible JSON value (serialized as string for stability).
    /// Since v2 every clip importer also reads [`LOSSLESS_CLIPS_KEY`] here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<String>,
    /// Sub-assets produced from this source.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sub_assets: Vec<SubAssetEntry>,
    /// Optional provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
}

impl MetaSidecar {
    /// Whether this source's own settings keep its animation clips lossless at cook
    /// ([`LOSSLESS_CLIPS_KEY`], ADR-7819). The cook reads the effective settings (folder rules
    /// merged in) through [`clip_cook_lossless`]; a malformed value reads as `false` here.
    #[must_use]
    pub fn lossless_clips(&self) -> bool {
        clip_cook_lossless(self.settings.as_deref()).unwrap_or(false)
    }

    /// New meta for a source about to be imported.
    pub fn new(importer: &str, id: AssetId) -> Self {
        Self {
            format_version: META_FORMAT_VERSION,
            id,
            importer: importer.to_string(),
            settings: None,
            sub_assets: Vec::new(),
            provenance: None,
        }
    }

    /// Parse RON from disk bytes.
    ///
    /// The `format_version` field is peeked before the full deserialise, so a *newer* version is
    /// rejected even when its body no longer matches the current struct. RON's `IMPLICIT_SOME`
    /// extension is enabled so the frozen v1 example's bare optionals (`settings: "…"` rather than
    /// `settings: Some("…")`) load.
    pub fn from_ron(bytes: &[u8]) -> Result<Self, MetaError> {
        let s = std::str::from_utf8(bytes).map_err(|e| MetaError::Ron(e.to_string()))?;
        let version = regia_core::version::peek_ron_version(s).map_err(|e| MetaError::Ron(e.to_string()))?;
        if version > META_FORMAT_VERSION {
            return Err(MetaError::UnsupportedVersion {
                found: version,
                supported: META_FORMAT_VERSION,
            });
        }
        let opts = ron::Options::default().with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME);
        let mut meta: Self = opts.from_str(s).map_err(|e| MetaError::Ron(e.to_string()))?;
        meta.migrate_to_latest()?;
        meta.validate()?;
        Ok(meta)
    }

    /// Serialize to canonical RON (sorted keys via `BTreeMap` fields in provenance only).
    pub fn to_ron_string(&self) -> Result<String, MetaError> {
        self.validate()?;
        ron::ser::to_string_pretty(self, regia_core::ron_pretty()).map_err(|e| MetaError::Ron(e.to_string()))
    }

    /// Validate version and required fields.
    pub fn validate(&self) -> Result<(), MetaError> {
        if self.format_version != META_FORMAT_VERSION {
            return Err(MetaError::UnsupportedVersion {
                found: self.format_version,
                supported: META_FORMAT_VERSION,
            });
        }
        if self.id.is_nil() {
            return Err(MetaError::MissingId);
        }
        if self.importer.is_empty() {
            return Err(MetaError::MissingImporter);
        }
        Ok(())
    }

    /// Reject a newer version, or run the registered migrator chain to bring an older file up to
    /// [`META_FORMAT_VERSION`] (`docs/specs/12-assets.md` § 4a).
    pub fn migrate_to_latest(&mut self) -> Result<u32, MetaError> {
        if self.format_version > META_FORMAT_VERSION {
            return Err(MetaError::UnsupportedVersion {
                found: self.format_version,
                supported: META_FORMAT_VERSION,
            });
        }
        if self.format_version < META_FORMAT_VERSION {
            let to = version::migrate(
                self,
                self.format_version,
                builtin_migrations(),
                META_FORMAT_VERSION,
            )
            .map_err(|e| MetaError::Migration(e.to_string()))?;
            self.format_version = to;
        }
        Ok(self.format_version)
    }
}

/// The built-in migration chain for `.meta`: `v1 → v2` (ADR-7819). The same chain runs in
/// `regia upgrade-project`.
fn builtin_migrations() -> &'static [VersionStep<MetaSidecar>] {
    &[VersionStep {
        from: 1,
        to: 2,
        migrate: migrate_v1_to_v2,
    }]
}

/// `.meta` v1 → v2: v2 importers read [`LOSSLESS_CLIPS_KEY`] from `settings`, v1 ones did not (the
/// glTF importer ignored the key with a warning, the BVH importer refused it). The step drops the
/// key, so a v1 file migrates with the switch off and cooks as it did; every other field keeps its
/// value text and order. Settings that do not parse as a RON object are left alone (no v1 importer
/// read the key from them either).
fn migrate_v1_to_v2(meta: &mut MetaSidecar) -> Result<(), String> {
    meta.settings = settings_without_lossless_clips(meta.settings.take());
    Ok(())
}

/// `settings` with the [`LOSSLESS_CLIPS_KEY`] field removed: unchanged when it has none or does not
/// parse, `None` when it was the only field.
fn settings_without_lossless_clips(settings: Option<String>) -> Option<String> {
    let text = settings?;
    let Ok(mut object) = crate::import_rules::SettingsObject::parse(&text) else {
        return Some(text);
    };
    let before = object.fields.len();
    object.fields.retain(|(name, _)| name != LOSSLESS_CLIPS_KEY);
    if object.fields.len() == before {
        Some(text)
    } else if object.fields.is_empty() {
        None
    } else {
        Some(object.to_ron())
    }
}

/// The versions a built-in migration step leaves (`from` side), for `cargo xtask check-formats`
/// to verify a version bump ships with a migrator from the previous version.
pub fn meta_migration_sources() -> Vec<u32> {
    builtin_migrations().iter().map(|s| s.from).collect()
}

/// The serde field-list fingerprint of the `.meta` format (hashed by `cargo xtask check-formats`).
///
/// The root struct *and* every nested struct are listed, so a shape change anywhere in the tree
/// changes the hash; the enums' variant names (canonical + alias spellings) cover wire renames.
pub fn meta_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<MetaSidecar>(),
        regia_core::schema::serde_field_names::<SubAssetEntry>(),
        regia_core::schema::serde_field_names::<Provenance>(),
        regia_core::schema::serde_variant_names::<ProvenanceTool>(),
        regia_core::schema::serde_variant_names::<ReviewerState>(),
        META_V2_SETTINGS_KEYS
            .iter()
            .map(|key| (*key).to_string())
            .collect(),
    ])
}

/// Path to the sidecar for a source asset.
pub fn meta_path_for(source: &Path) -> PathBuf {
    let mut buf = source.to_path_buf();
    let file_name = source
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();
    buf.set_file_name(format!("{file_name}.meta"));
    buf
}

/// Errors working with `.meta` files.
#[derive(Debug, Error)]
pub enum MetaError {
    /// RON parse/serialize failure.
    #[error("meta ron error: {0}")]
    Ron(String),

    /// Unsupported `format_version`.
    #[error("unsupported meta format version {found} (supported: {supported})")]
    UnsupportedVersion {
        /// Version in file.
        found: u32,
        /// Supported version.
        supported: u32,
    },

    /// Missing asset id.
    #[error("meta is missing id")]
    MissingId,

    /// Missing importer name.
    #[error("meta is missing importer")]
    MissingImporter,

    /// A migration step failed or is missing.
    #[error("meta migration failed: {0}")]
    Migration(String),

    /// IO error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
