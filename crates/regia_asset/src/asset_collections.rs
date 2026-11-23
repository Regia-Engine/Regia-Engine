// SPDX-License-Identifier: MPL-2.0
//! Asset collections and Content Browser settings (`config/asset_collections.ron`, format
//! `asset-collections` v1; `docs/formats/asset-collections-ron.md`, ADR-7759).
//!
//! A collection groups assets without moving files: a static collection is a named list of asset
//! ids (or project-relative paths for files with no `.meta`), a dynamic collection is a saved
//! Content Browser query. The same format holds favorite folders, folder colors and browser
//! settings. The project's shared file is `config/asset_collections.ron`; each user's personal
//! file has the same shape and lives in the project's user-local `.regia/asset_collections.ron`
//! (ignored by version control; ADR-7902).
//!
//! ```ron
//! (
//!     format_version: 1,
//!     collections: [
//!         (name: "Hero props", assets: ["7c9e6679-7425-40de-944b-e07fc1f90ae7", "assets/props/crate.glb"]),
//!         (name: "Needs review", query: Some("is:review type:mesh")),
//!     ],
//!     favorites: ["assets/props"],
//!     folder_colors: {"assets/props": "signal"},
//!     hide_empty_folders: false,
//! )
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Current `asset-collections` format version.
pub const ASSET_COLLECTIONS_FORMAT_VERSION: u32 = 1;

/// Project-relative path of the shared collections file.
pub const SHARED_COLLECTIONS_PATH: &str = "config/asset_collections.ron";

/// Longest collection name accepted, in bytes.
pub const MAX_COLLECTION_NAME: usize = 128;

/// One collection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetCollection {
    /// The name (unique in its file, compared ignoring case).
    pub name: String,
    /// A static collection's members: asset ids, or project-relative paths for files with no
    /// `.meta`. Empty for a dynamic collection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assets: Vec<String>,
    /// A dynamic collection's saved search (Content Browser query syntax); `None` for a static
    /// collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
}

impl AssetCollection {
    /// Whether this is a saved query.
    #[must_use]
    pub fn is_dynamic(&self) -> bool {
        self.query.is_some()
    }
}

/// The collections file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetCollectionsFile {
    /// Must be [`ASSET_COLLECTIONS_FORMAT_VERSION`].
    pub format_version: u32,
    /// Collections in display order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collections: Vec<AssetCollection>,
    /// Favorite folders (project-relative, `assets/...`) in display order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub favorites: Vec<String>,
    /// Folder (project-relative) → editor theme color token name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub folder_colors: BTreeMap<String, String>,
    /// Hide folders with no asset anywhere under them in the folder tree.
    #[serde(default)]
    pub hide_empty_folders: bool,
}

impl Default for AssetCollectionsFile {
    fn default() -> Self {
        Self {
            format_version: ASSET_COLLECTIONS_FORMAT_VERSION,
            collections: Vec::new(),
            favorites: Vec::new(),
            folder_colors: BTreeMap::new(),
            hide_empty_folders: false,
        }
    }
}

/// Why a collections file could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionsError(pub String);

impl std::fmt::Display for CollectionsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CollectionsError {}

/// Why `name` is not a valid collection name (`None` when it is): not blank, at most
/// [`MAX_COLLECTION_NAME`] bytes, no control characters.
#[must_use]
pub fn collection_name_problem(name: &str) -> Option<&'static str> {
    if name.trim().is_empty() {
        Some("is empty")
    } else if name.len() > MAX_COLLECTION_NAME {
        Some("is too long")
    } else if name.chars().any(char::is_control) {
        Some("contains a control character")
    } else {
        None
    }
}

impl AssetCollectionsFile {
    /// Parses a collections file: the version must be current, names valid and unique (ignoring
    /// case), and a collection either static or dynamic, not both.
    ///
    /// # Errors
    ///
    /// [`CollectionsError`] naming the problem.
    pub fn from_ron(text: &str) -> Result<Self, CollectionsError> {
        let version =
            regia_core::version::peek_ron_version(text).map_err(|e| CollectionsError(e.to_string()))?;
        if version != ASSET_COLLECTIONS_FORMAT_VERSION {
            return Err(CollectionsError(format!(
                "format_version {version} is not supported (expected {ASSET_COLLECTIONS_FORMAT_VERSION})"
            )));
        }
        let file: Self = ron::from_str(text).map_err(|e| CollectionsError(e.to_string()))?;
        file.validate()?;
        Ok(file)
    }

    /// Checks names and kinds (see [`Self::from_ron`]).
    ///
    /// # Errors
    ///
    /// [`CollectionsError`] naming the first problem.
    pub fn validate(&self) -> Result<(), CollectionsError> {
        let mut seen = std::collections::BTreeSet::new();
        for (i, collection) in self.collections.iter().enumerate() {
            if let Some(problem) = collection_name_problem(&collection.name) {
                return Err(CollectionsError(format!(
                    "collection {i}: the name {:?} {problem}",
                    collection.name
                )));
            }
            if !seen.insert(collection.name.trim().to_lowercase()) {
                return Err(CollectionsError(format!(
                    "collection {i}: the name {:?} is used twice",
                    collection.name
                )));
            }
            if collection.query.is_some() && !collection.assets.is_empty() {
                return Err(CollectionsError(format!(
                    "collection {:?} has both a query and assets: a collection is a saved query or a list",
                    collection.name
                )));
            }
        }
        Ok(())
    }

    /// The RON text of this file (pretty, deterministic).
    ///
    /// # Errors
    ///
    /// [`CollectionsError`] when serialization fails.
    pub fn to_ron_string(&self) -> Result<String, CollectionsError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty().struct_names(false))
            .map_err(|e| CollectionsError(e.to_string()))
    }

    /// The collection named `name` (ignoring case).
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&AssetCollection> {
        let name = name.trim().to_lowercase();
        self.collections
            .iter()
            .find(|c| c.name.trim().to_lowercase() == name)
    }

    /// The collection named `name` (ignoring case), mutable.
    pub fn find_mut(&mut self, name: &str) -> Option<&mut AssetCollection> {
        let name = name.trim().to_lowercase();
        self.collections
            .iter_mut()
            .find(|c| c.name.trim().to_lowercase() == name)
    }
}

/// The serde shape of `asset_collections.ron` for `cargo xtask check-formats`
/// (`docs/formats/asset-collections-ron.md`, ADR-7759).
#[must_use]
pub fn asset_collections_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<AssetCollectionsFile>(),
        regia_core::schema::serde_field_names::<AssetCollection>(),
    ])
}

/// `asset_collections.ron` is at its first version: no migration steps.
#[must_use]
pub fn asset_collections_migration_sources() -> Vec<u32> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_documented_example_round_trips() {
        let text = r#"(
            format_version: 1,
            collections: [
                (name: "Hero props", assets: ["7c9e6679-7425-40de-944b-e07fc1f90ae7", "assets/props/crate.glb"]),
                (name: "Needs review", query: Some("is:review type:mesh")),
            ],
            favorites: ["assets/props"],
            folder_colors: {"assets/props": "signal"},
            hide_empty_folders: false,
        )"#;
        let file = AssetCollectionsFile::from_ron(text).expect("parses");
        assert_eq!(file.collections.len(), 2);
        assert!(file.collections[1].is_dynamic());
        assert_eq!(file.find("hero PROPS").map(|c| c.assets.len()), Some(2));
        let again = AssetCollectionsFile::from_ron(&file.to_ron_string().expect("ron")).expect("again");
        assert_eq!(again, file);
        let empty = AssetCollectionsFile::from_ron("(format_version: 1)").expect("minimal");
        assert_eq!(empty, AssetCollectionsFile::default());
    }

    #[test]
    fn bad_files_say_why() {
        for (text, needle) in [
            ("(format_version: 2)", "not supported"),
            ("(format_version: 1, collections: [(name: \" \")])", "is empty"),
            (
                "(format_version: 1, collections: [(name: \"A\"), (name: \"a\")])",
                "used twice",
            ),
            (
                "(format_version: 1, collections: [(name: \"A\", assets: [\"x\"], query: Some(\"y\"))])",
                "both a query and assets",
            ),
            ("(format_version: 1, extra: 1)", "extra"),
        ] {
            let err = AssetCollectionsFile::from_ron(text).expect_err(text);
            assert!(err.0.contains(needle), "{text}: {err}");
        }
    }
}
