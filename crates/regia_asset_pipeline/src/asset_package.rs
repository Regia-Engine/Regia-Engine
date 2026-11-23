// SPDX-License-Identifier: MPL-2.0
//! The manifest of an asset package: `.regia_assetpkg.ron` v1 (spec addendum § 8.9, ADR-7788,
//! `docs/formats/assetpkg-ron.md`).
//!
//! An asset package is a folder that holds some assets of a project with their dependencies, at the
//! same project-relative paths (`assets/...`) and with their `.meta` sidecars (so the asset ids stay),
//! plus this manifest at its top. The editor writes it (Export with dependencies…) and reads it
//! (Import package…), refusing to import over an existing path or an asset id the project already
//! uses. The manifest only lists; the files are the assets themselves.

use serde::{Deserialize, Serialize};

/// The manifest's file name at the top of a package folder.
pub const ASSET_PACKAGE_MANIFEST: &str = ".regia_assetpkg.ron";

/// The current manifest version.
pub const ASSET_PACKAGE_FORMAT_VERSION: u32 = 1;

/// Largest manifest the loader reads (bytes).
pub const MAX_MANIFEST_BYTES: usize = 16 << 20;

/// `.regia_assetpkg.ron` v1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetPackageManifest {
    /// Always [`ASSET_PACKAGE_FORMAT_VERSION`] when written.
    pub format_version: u32,
    /// The assets the user picked (project-relative); the rest of [`Self::assets`] is their closure.
    pub roots: Vec<String>,
    /// Every packaged asset, sorted by path.
    pub assets: Vec<AssetPackageEntry>,
    /// When the package was written (RFC 3339, UTC), if known.
    #[serde(default)]
    pub created_utc: Option<String>,
}

/// One packaged asset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetPackageEntry {
    /// Project-relative path, `/`-separated, under `assets/`.
    pub path: String,
    /// The asset id from its `.meta` (`None`: the asset has no sidecar).
    #[serde(default)]
    pub id: Option<String>,
    /// File length in bytes.
    pub bytes: u64,
    /// FNV-1a 64-bit hash of the file, 16 lowercase hex digits (a transfer check, not a signature).
    pub hash: String,
}

/// Why a manifest did not load.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AssetPackageError {
    /// Not RON of the v1 shape.
    #[error("the package manifest does not parse: {0}")]
    Parse(String),
    /// A version this build does not read.
    #[error("package manifest version {0} is newer than this build reads ({ASSET_PACKAGE_FORMAT_VERSION})")]
    Version(u32),
    /// A path that is not a plain path under `assets/`.
    #[error("the package lists {0:?}, which is not a path under assets/")]
    BadPath(String),
    /// Over [`MAX_MANIFEST_BYTES`].
    #[error("the package manifest is larger than {MAX_MANIFEST_BYTES} bytes")]
    TooLarge,
}

impl AssetPackageError {
    /// Stable lowercase code for the agent protocol.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse(_) => "pkg_bad_manifest",
            Self::Version(_) => "pkg_version",
            Self::BadPath(_) => "pkg_bad_path",
            Self::TooLarge => "pkg_too_large",
        }
    }
}

/// Whether `path` is a plain project-relative path under `assets/` (no `..`, no drive or root, `/` only).
#[must_use]
pub fn is_package_path(path: &str) -> bool {
    path.starts_with("assets/")
        && !path.contains('\\')
        && !path.contains(':')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

impl AssetPackageManifest {
    /// Parses and checks a manifest.
    ///
    /// # Errors
    ///
    /// See [`AssetPackageError`].
    pub fn from_ron(text: &str) -> Result<Self, AssetPackageError> {
        if text.len() > MAX_MANIFEST_BYTES {
            return Err(AssetPackageError::TooLarge);
        }
        let manifest: Self = ron::from_str(text).map_err(|e| AssetPackageError::Parse(e.to_string()))?;
        if manifest.format_version > ASSET_PACKAGE_FORMAT_VERSION || manifest.format_version == 0 {
            return Err(AssetPackageError::Version(manifest.format_version));
        }
        for path in manifest
            .assets
            .iter()
            .map(|entry| &entry.path)
            .chain(&manifest.roots)
        {
            if !is_package_path(path) {
                return Err(AssetPackageError::BadPath(path.clone()));
            }
        }
        Ok(manifest)
    }

    /// The manifest as pretty RON.
    ///
    /// # Errors
    ///
    /// The serializer failed (does not happen for this shape).
    pub fn to_ron(&self) -> Result<String, AssetPackageError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| AssetPackageError::Parse(e.to_string()))
    }
}

/// The serde shape of `.regia_assetpkg.ron` for `cargo xtask check-formats`.
#[must_use]
pub fn asset_package_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    join_field_lists(&[
        serde_field_names::<AssetPackageManifest>(),
        serde_field_names::<AssetPackageEntry>(),
    ])
}

/// `.regia_assetpkg.ron` is at its first version: no migration steps.
#[must_use]
pub fn asset_package_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// FNV-1a 64-bit of `bytes` as 16 lowercase hex digits (Fowler, Noll and Vo; public domain).
#[must_use]
pub fn package_hash(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// A v1 manifest for tests, fuzzing and the format corpus.
#[must_use]
pub fn fixture_manifest() -> AssetPackageManifest {
    AssetPackageManifest {
        format_version: ASSET_PACKAGE_FORMAT_VERSION,
        roots: vec!["assets/props/crate.prefab.ron".to_string()],
        assets: vec![
            AssetPackageEntry {
                path: "assets/props/crate.glb".to_string(),
                id: Some("5f1d3c9a-0b7e-4c1a-9d0e-2a6b8c4f1e30".to_string()),
                bytes: 1024,
                hash: package_hash(b"crate"),
            },
            AssetPackageEntry {
                path: "assets/props/crate.prefab.ron".to_string(),
                id: None,
                bytes: 96,
                hash: package_hash(b"prefab"),
            },
        ],
        created_utc: Some("2026-11-15T00:00:00Z".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trips_and_checks_paths_and_versions() {
        let manifest = fixture_manifest();
        let text = manifest.to_ron().expect("ron");
        assert_eq!(AssetPackageManifest::from_ron(&text).expect("parse"), manifest);
        let newer = text.replace("format_version: 1", "format_version: 2");
        assert_eq!(
            AssetPackageManifest::from_ron(&newer).map_err(|e| e.code()),
            Err("pkg_version")
        );
        for bad in [
            "assets/../secret.ron",
            "/etc/passwd",
            "props/crate.glb",
            "assets/a\\\\b.png", // RON text `a\\b`: one backslash in the parsed path
            "C:/x",
        ] {
            let edited = text.replace("assets/props/crate.glb", bad);
            assert_eq!(
                AssetPackageManifest::from_ron(&edited).map_err(|e| e.code()),
                Err("pkg_bad_path"),
                "{bad}"
            );
        }
        assert_eq!(
            AssetPackageManifest::from_ron("(nonsense").map_err(|e| e.code()),
            Err("pkg_bad_manifest")
        );
        assert!(asset_package_schema_fingerprint().contains("format_version"));
        assert_eq!(package_hash(b""), "cbf29ce484222325");
    }

    /// Cut and byte-flipped manifests are refused with an error, never a panic (the fuzz coverage of
    /// the `assetpkg` format; `regia_agent/tests/fuzz_targets.rs` names this test).
    #[test]
    fn manifest_survives_mutation() {
        let text = fixture_manifest().to_ron().expect("ron");
        let bytes = text.as_bytes();
        let mut state: u64 = 0x5eed_7788;
        for round in 0..2000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let mut mutated = bytes.to_vec();
            let at = (state as usize) % mutated.len();
            match round % 3 {
                0 => mutated.truncate(at),
                1 => mutated[at] = (state >> 32) as u8,
                _ => mutated.insert(at, b'"'),
            }
            if let Ok(text) = std::str::from_utf8(&mutated) {
                let _ = AssetPackageManifest::from_ron(text);
            }
        }
    }
}
