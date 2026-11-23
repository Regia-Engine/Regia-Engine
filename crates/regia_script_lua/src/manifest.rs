// SPDX-License-Identifier: MPL-2.0
//! `luamod.ron`: a mod's manifest (`docs/specs/84-lua-mods.md` § 3, format `luamod`).

use serde::{Deserialize, Serialize};

use crate::API_VERSION;

/// The manifest's file name inside a mod.
pub const LUAMOD_FILE: &str = "luamod.ron";

/// The current `luamod.ron` `format_version`. Version 1 is the first: no migration step exists yet.
pub const LUAMOD_FORMAT_VERSION: u32 = 1;

/// The longest mod id, in bytes.
const MAX_ID_BYTES: usize = 64;
/// The longest `name` and `version`, in bytes.
const MAX_LABEL_BYTES: usize = 256;
/// Ids starting with this belong to the host: spawns of `lua.run` scratch states use one.
const RESERVED_ID_PREFIX: &str = "agent.";

/// A mod's manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LuaModManifest {
    /// `luamod.ron` version ([`LUAMOD_FORMAT_VERSION`]).
    pub format_version: u32,
    /// Stable id: `[a-z0-9_.-]`, 1 to 64 bytes. Mods run in id order.
    pub id: String,
    /// Display name.
    pub name: String,
    /// The mod's own version string (free form).
    pub version: String,
    /// The entry chunk, a path relative to the mod (e.g. `main.lua`).
    pub entry: String,
    /// The `regia` table version the mod was written for ([`API_VERSION`]).
    pub api_version: u32,
    /// Instruction budget per call the mod asks for; clamped to the host's maximum.
    #[serde(default)]
    pub instruction_budget: Option<u64>,
    /// Memory cap the mod asks for, in bytes; clamped to the host's maximum.
    #[serde(default)]
    pub memory_limit_bytes: Option<u64>,
}

/// Why a manifest was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    /// The RON did not parse into the v1 shape.
    #[error("luamod.ron does not parse: {0}")]
    Parse(String),
    /// A `format_version` newer than this build reads.
    #[error("luamod.ron format_version {found} is newer than this engine reads ({LUAMOD_FORMAT_VERSION})")]
    NewerVersion {
        /// The file's version.
        found: u32,
    },
    /// `format_version` 0 is not a version.
    #[error("luamod.ron format_version 0 is not a version")]
    ZeroVersion,
    /// The `api_version` is not one this build provides.
    #[error("api_version {found} is not provided by this engine (it provides {API_VERSION})")]
    UnknownApi {
        /// The file's API version.
        found: u32,
    },
    /// The id is empty, too long, has characters outside `[a-z0-9_.-]` or starts with `agent.`.
    #[error(
        "mod id `{0}` must be 1 to 64 characters of a-z, 0-9, `_`, `.` or `-`, not starting with `agent.`"
    )]
    BadId(String),
    /// The entry path is absolute, has `..`, a backslash, or does not end in `.lua`.
    #[error("entry `{0}` must be a relative `.lua` path inside the mod")]
    BadEntry(String),
    /// `name` or `version` is longer than 256 bytes; the field's name.
    #[error("`{0}` is at most {MAX_LABEL_BYTES} bytes")]
    LabelTooLong(&'static str),
}

/// Reads only `format_version` (a bare number, absent allowed); every other field is skipped.
#[derive(Deserialize)]
struct VersionProbe {
    #[serde(default, deserialize_with = "bare_version")]
    format_version: Option<u32>,
}

/// `format_version: 1` is a bare number in the file, not RON's `Some(1)`.
fn bare_version<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<u32>, D::Error> {
    u32::deserialize(deserializer).map(Some)
}

impl LuaModManifest {
    /// Parses and validates `luamod.ron` text. The version is read first, so a newer file is
    /// refused as newer rather than for its unknown fields.
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        let options = ron::Options::default();
        let probe: VersionProbe = options
            .from_str(text)
            .map_err(|e| ManifestError::Parse(e.to_string()))?;
        match probe.format_version {
            Some(0) => return Err(ManifestError::ZeroVersion),
            Some(found) if found > LUAMOD_FORMAT_VERSION => {
                return Err(ManifestError::NewerVersion { found });
            }
            _ => {}
        }
        let manifest: LuaModManifest = options
            .from_str(text)
            .map_err(|e| ManifestError::Parse(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Checks the id, entry and API version.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.api_version != API_VERSION {
            return Err(ManifestError::UnknownApi {
                found: self.api_version,
            });
        }
        if !valid_id(&self.id) {
            return Err(ManifestError::BadId(self.id.clone()));
        }
        if !crate::files::is_safe_relative(&self.entry) || !self.entry.ends_with(".lua") {
            return Err(ManifestError::BadEntry(self.entry.clone()));
        }
        if self.name.len() > MAX_LABEL_BYTES {
            return Err(ManifestError::LabelTooLong("name"));
        }
        if self.version.len() > MAX_LABEL_BYTES {
            return Err(ManifestError::LabelTooLong("version"));
        }
        Ok(())
    }
}

/// Whether `id` is a valid mod id. The `agent.` prefix is reserved: `lua.run`'s throwaway state
/// owns entities under `agent.scratch`, and ownership is by id.
pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && !id.starts_with(RESERVED_ID_PREFIX)
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-'))
}

/// The `from` versions of the registered migration steps (none: v1 is the first version).
#[must_use]
pub fn luamod_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde field-list fingerprint of `luamod.ron` (hashed by `cargo xtask check-formats`).
#[must_use]
pub fn luamod_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[regia_core::schema::serde_field_names::<LuaModManifest>()])
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1: &str = r#"(
        format_version: 1,
        id: "example.lights",
        name: "Blinking lights",
        version: "1.0.0",
        entry: "main.lua",
        api_version: 1,
    )"#;

    #[test]
    fn a_v1_manifest_loads_with_optional_budgets_absent() {
        let m = LuaModManifest::parse(V1).expect("v1 parses");
        assert_eq!(m.id, "example.lights");
        assert_eq!(m.instruction_budget, None);
        assert_eq!(m.memory_limit_bytes, None);
    }

    #[test]
    fn budgets_parse_when_present() {
        let text = V1.replace(
            "api_version: 1,",
            "api_version: 1, instruction_budget: Some(5000), memory_limit_bytes: Some(1048576),",
        );
        let m = LuaModManifest::parse(&text).expect("parses");
        assert_eq!(m.instruction_budget, Some(5000));
        assert_eq!(m.memory_limit_bytes, Some(1_048_576));
    }

    #[test]
    fn a_newer_version_is_refused_as_newer_even_with_new_fields() {
        let text = V1
            .replace("format_version: 1", "format_version: 2")
            .replace("api_version: 1,", "api_version: 1, icon: \"icon.png\",");
        assert_eq!(
            LuaModManifest::parse(&text),
            Err(ManifestError::NewerVersion { found: 2 })
        );
    }

    #[test]
    fn unknown_fields_bad_ids_entries_and_apis_are_refused() {
        let unknown = V1.replace("api_version: 1,", "api_version: 1, extra: 3,");
        assert!(matches!(
            LuaModManifest::parse(&unknown),
            Err(ManifestError::Parse(_))
        ));
        for id in [
            "",
            "Upper",
            "has space",
            "slash/no",
            "agent.scratch",
            &"x".repeat(65),
        ] {
            let text = V1.replace("example.lights", id);
            assert!(
                matches!(LuaModManifest::parse(&text), Err(ManifestError::BadId(_))),
                "{id}"
            );
        }
        for entry in [
            "../main.lua",
            "/abs.lua",
            // RON escapes: the value is `dir\main.lua`.
            "dir\\\\main.lua",
            "main.txt",
            "a/../../b.lua",
            "",
        ] {
            let text = V1.replace("main.lua", entry);
            assert!(
                matches!(LuaModManifest::parse(&text), Err(ManifestError::BadEntry(_))),
                "{entry}"
            );
        }
        let long = V1.replace("Blinking lights", &"x".repeat(257));
        assert_eq!(
            LuaModManifest::parse(&long),
            Err(ManifestError::LabelTooLong("name"))
        );
        let api = V1.replace("api_version: 1", "api_version: 9");
        assert_eq!(
            LuaModManifest::parse(&api),
            Err(ManifestError::UnknownApi { found: 9 })
        );
        let zero = V1.replace("format_version: 1", "format_version: 0");
        assert_eq!(LuaModManifest::parse(&zero), Err(ManifestError::ZeroVersion));
        let missing = V1.replace("format_version: 1,", "");
        assert!(matches!(
            LuaModManifest::parse(&missing),
            Err(ManifestError::Parse(_))
        ));
    }

    #[test]
    fn the_fingerprint_lists_every_field() {
        let fp = luamod_schema_fingerprint();
        for field in [
            "format_version",
            "id",
            "entry",
            "api_version",
            "instruction_budget",
            "memory_limit_bytes",
        ] {
            assert!(fp.contains(field), "{fp}");
        }
        assert!(luamod_migration_sources().is_empty());
    }
}
