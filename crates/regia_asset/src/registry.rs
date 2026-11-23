// SPDX-License-Identifier: MPL-2.0
//! Asset registry: queries over every cooked asset's path, type, tags and dependencies without
//! loading any asset (`docs/specs/content-data.md` § 2).
//!
//! The cook writes one record per manifest entry into `asset_registry.bin` in the cooked cache
//! (`regia_asset_pipeline::registry_build`); packaging copies a per-pak registry next to each pak
//! (`base.fpak` → `base.registry.bin`). [`AssetRegistry`] holds the records sorted by path plus a
//! reverse dependency index, so [`AssetRegistry::query`], [`AssetRegistry::dependencies`] and
//! [`AssetRegistry::referencers`] never open a cooked file. [`AssetRegistry::apply_changes`] updates
//! single records and their reverse-index edges, which is how the live cook keeps the registry
//! current without a full rebuild.
//!
//! File layout (format `asset_registry` v1): the 8-byte magic [`REGISTRY_MAGIC`], the format
//! version as a little-endian `u32`, then the postcard-encoded record list sorted by path.

mod tags;

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::id::AssetId;

pub use tags::{
    MAX_TAG_LEN, TAG_RULES_FORMAT_VERSION, TAG_RULES_PATH, TagRule, TagRules, automatic_tags,
    path_glob_match, source_path, tag_key_problem,
};

/// Current `asset_registry` format version.
pub const REGISTRY_FORMAT_VERSION: u32 = 1;

/// First bytes of a registry file.
pub const REGISTRY_MAGIC: [u8; 8] = *b"RGASREG\0";

/// File name of the registry in a cooked cache directory.
pub const REGISTRY_FILE_NAME: &str = "asset_registry.bin";

/// Suffix of the registry written next to a pak (`base.fpak` → `base.registry.bin`).
pub const PAK_REGISTRY_SUFFIX: &str = ".registry.bin";

/// The serde shape of the asset registry file (`asset_registry.bin`, `<pak>.registry.bin`) for
/// `cargo xtask check-formats` (`docs/formats/asset-registry.md`, ADR-2316): the record fields in
/// their postcard order, after the 12-byte header (`REGISTRY_MAGIC`, `u32` LE version).
#[must_use]
pub fn registry_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[regia_core::schema::serde_field_names::<RegistryEntry>()])
}

/// The registry file is at its first version: no migration steps (a cache: an older file is
/// cooked again).
#[must_use]
pub fn registry_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde shape of `config/asset_tags.ron` for `cargo xtask check-formats`
/// (`docs/formats/asset-tags-ron.md`, ADR-2316).
#[must_use]
pub fn tag_rules_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<TagRules>(),
        regia_core::schema::serde_field_names::<TagRule>(),
    ])
}

/// `config/asset_tags.ron` is at its first version: no migration steps.
#[must_use]
pub fn tag_rules_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// One cooked asset as the registry records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryEntry {
    /// Asset id (unique in a registry).
    pub id: AssetId,
    /// Project-relative path from the manifest (`assets/a.glb`, `assets/a.glb#mesh/Body`).
    pub path: String,
    /// Reflect path of the cooked type.
    pub type_path: String,
    /// Assets this one references (from the cooked header), sorted, without duplicates.
    pub dependencies: Vec<AssetId>,
    /// Tags: automatic (`ext`, `folder`, `importer`) and authored (`config/asset_tags.ron`).
    pub tags: BTreeMap<String, String>,
    /// Cooked file length in bytes (0 when unknown).
    pub size: u64,
}

/// One edit for [`AssetRegistry::apply_changes`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryChange {
    /// Insert the record, or replace the record with the same id (and any record at its path).
    Upsert(RegistryEntry),
    /// Remove the record at this path.
    RemovePath(String),
    /// Remove the record of a source and all its sub-assets (`path` and `path#...`).
    RemoveSource(String),
}

/// A tag condition of a [`RegistryQuery`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TagFilter {
    /// The tag is present (any value).
    Has(String),
    /// The tag has exactly this value.
    Equals(String, String),
}

impl TagFilter {
    /// Parses `key` or `key=value`.
    ///
    /// # Errors
    ///
    /// The key is not a valid tag key ([`tag_key_problem`]).
    pub fn parse(text: &str) -> Result<Self, RegistryError> {
        let filter = match text.split_once('=') {
            Some((k, v)) => Self::Equals(k.trim().to_string(), v.trim().to_string()),
            None => Self::Has(text.trim().to_string()),
        };
        let key = filter.key();
        match tag_key_problem(key) {
            Some(problem) => Err(RegistryError::BadQuery(format!("tag key {key:?} {problem}"))),
            None => Ok(filter),
        }
    }

    /// The tag key this filter tests.
    pub fn key(&self) -> &str {
        match self {
            Self::Has(k) | Self::Equals(k, _) => k,
        }
    }

    fn matches(&self, tags: &BTreeMap<String, String>) -> bool {
        match self {
            Self::Has(k) => tags.contains_key(k),
            Self::Equals(k, v) => tags.get(k) == Some(v),
        }
    }
}

/// A registry search: every set condition must hold (AND).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegistryQuery {
    /// Path glob ([`path_glob_match`]) over the full registry path.
    pub path: Option<String>,
    /// Substring of the path, any ASCII case.
    pub path_contains: Option<String>,
    /// Type: the full reflect path, or its trailing segments (`CookedTexture`, or `regia_asset::CookedTexture` in full); a partial leading segment such as `asset::CookedTexture` does not match, because the remainder must end at a `::` boundary.
    pub type_path: Option<String>,
    /// Tag conditions.
    pub tags: Vec<TagFilter>,
    /// Most records returned (all when `None`); [`RegistryMatches::total`] still counts every match.
    pub limit: Option<usize>,
}

/// The answer of [`AssetRegistry::query`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryMatches<'a> {
    /// Every matching record count, before the limit.
    pub total: usize,
    /// The first `limit` matches, sorted by path.
    pub entries: Vec<&'a RegistryEntry>,
}

/// The answer of a dependency or referencer walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryWalk<'a> {
    /// Records reached, breadth-first, sorted by path within each level, each once.
    pub entries: Vec<&'a RegistryEntry>,
    /// Ids named as dependencies that the registry has no record for, sorted.
    pub missing: Vec<AssetId>,
}

/// Errors reading, writing or querying a registry.
#[derive(Debug, Error)]
pub enum RegistryError {
    /// The file does not start with [`REGISTRY_MAGIC`] or is shorter than its header.
    #[error("not an asset registry file (bad magic or truncated header)")]
    BadMagic,
    /// The file's version is newer than this build reads.
    #[error("unsupported asset registry version {found} (supported: {supported})")]
    UnsupportedVersion {
        /// Version in the file.
        found: u32,
        /// Highest version this build reads.
        supported: u32,
    },
    /// The record list does not decode.
    #[error("postcard error: {0}")]
    Postcard(#[from] postcard::Error),
    /// IO error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The tag rules file is invalid.
    #[error("asset tag rules: {0}")]
    TagRules(String),
    /// A query parameter is invalid.
    #[error("bad registry query: {0}")]
    BadQuery(String),
}

/// The registry: records by path, an id index and a reverse dependency index.
#[derive(Clone, Debug, Default)]
pub struct AssetRegistry {
    by_path: BTreeMap<String, RegistryEntry>,
    path_of: HashMap<AssetId, String>,
    /// dependency id → ids of the records that name it.
    referencers: HashMap<AssetId, BTreeSet<AssetId>>,
    /// Bumped by every [`AssetRegistry::apply_changes`] that changed something.
    pub generation: u64,
}

impl PartialEq for AssetRegistry {
    /// Same records and same reverse index; [`AssetRegistry::generation`] is ignored.
    fn eq(&self, other: &Self) -> bool {
        self.by_path == other.by_path
            && self.path_of == other.path_of
            && self.referencers == other.referencers
    }
}

impl AssetRegistry {
    /// Builds a registry from records. A later record with the same id or path replaces the earlier.
    pub fn from_entries(entries: impl IntoIterator<Item = RegistryEntry>) -> Self {
        let mut registry = Self::default();
        for entry in entries {
            registry.upsert(entry);
        }
        registry.generation = 0;
        registry
    }

    /// Number of records.
    pub fn len(&self) -> usize {
        self.by_path.len()
    }

    /// True when the registry has no records.
    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }

    /// Every record, sorted by path.
    pub fn entries(&self) -> impl Iterator<Item = &RegistryEntry> {
        self.by_path.values()
    }

    /// The record of `id`.
    pub fn by_id(&self, id: AssetId) -> Option<&RegistryEntry> {
        self.path_of.get(&id).and_then(|p| self.by_path.get(p))
    }

    /// The record at a registry path (`assets/` prefix included).
    pub fn by_path(&self, path: &str) -> Option<&RegistryEntry> {
        self.by_path.get(path)
    }

    /// Resolves an asset id written as text, a full path, or a path without the `assets/` prefix.
    pub fn resolve(&self, asset: &str) -> Option<&RegistryEntry> {
        if let Ok(id) = asset.parse::<AssetId>() {
            return self.by_id(id);
        }
        let path = asset.replace('\\', "/");
        self.by_path(&path)
            .or_else(|| self.by_path(&format!("assets/{}", path.trim_start_matches('/'))))
    }

    /// Records matching every condition of `query`, sorted by path.
    pub fn query(&self, query: &RegistryQuery) -> RegistryMatches<'_> {
        let _span =
            tracing::debug_span!(target: "regia_asset", "registry_query", records = self.len()).entered();
        let needle = query.path_contains.as_ref().map(|s| s.to_ascii_lowercase());
        let mut total = 0;
        let mut entries = Vec::new();
        let limit = query.limit.unwrap_or(usize::MAX);
        for entry in self.by_path.values() {
            if let Some(glob) = &query.path
                && !path_glob_match(glob, &entry.path)
            {
                continue;
            }
            if let Some(needle) = &needle
                && !entry.path.to_ascii_lowercase().contains(needle.as_str())
            {
                continue;
            }
            if let Some(ty) = &query.type_path
                && !type_matches(&entry.type_path, ty)
            {
                continue;
            }
            if !query.tags.iter().all(|t| t.matches(&entry.tags)) {
                continue;
            }
            total += 1;
            if entries.len() < limit {
                entries.push(entry);
            }
        }
        RegistryMatches { total, entries }
    }

    /// What `id` depends on: its direct dependencies, or with `transitive` everything reachable.
    pub fn dependencies(&self, id: AssetId, transitive: bool) -> RegistryWalk<'_> {
        self.walk(id, transitive, |reg, at| {
            reg.by_id(at).map(|e| e.dependencies.clone()).unwrap_or_default()
        })
    }

    /// What references `id`: the records that name it as a dependency, or with `transitive` every
    /// record that reaches it.
    pub fn referencers(&self, id: AssetId, transitive: bool) -> RegistryWalk<'_> {
        self.walk(id, transitive, |reg, at| {
            reg.referencers
                .get(&at)
                .map(|s| s.iter().copied().collect())
                .unwrap_or_default()
        })
    }

    fn walk(
        &self,
        start: AssetId,
        transitive: bool,
        next: impl Fn(&Self, AssetId) -> Vec<AssetId>,
    ) -> RegistryWalk<'_> {
        let mut seen: BTreeSet<AssetId> = BTreeSet::from([start]);
        let mut missing = BTreeSet::new();
        let mut entries = Vec::new();
        let mut frontier: VecDeque<AssetId> = VecDeque::from([start]);
        while !frontier.is_empty() {
            let mut level: Vec<&RegistryEntry> = Vec::new();
            for at in frontier.drain(..) {
                for id in next(self, at) {
                    if !seen.insert(id) {
                        continue;
                    }
                    match self.by_id(id) {
                        Some(entry) => level.push(entry),
                        None => {
                            missing.insert(id);
                        }
                    }
                }
            }
            level.sort_by(|a, b| a.path.cmp(&b.path));
            if transitive {
                frontier.extend(level.iter().map(|e| e.id));
            }
            entries.extend(level);
        }
        RegistryWalk {
            entries,
            missing: missing.into_iter().collect(),
        }
    }

    /// Every tag key with how many records carry it, sorted by key.
    pub fn tag_keys(&self) -> BTreeMap<&str, usize> {
        let mut out = BTreeMap::new();
        for entry in self.by_path.values() {
            for key in entry.tags.keys() {
                *out.entry(key.as_str()).or_insert(0) += 1;
            }
        }
        out
    }

    /// Every value of tag `key` with how many records carry it, sorted by value.
    pub fn tag_values(&self, key: &str) -> BTreeMap<&str, usize> {
        let mut out = BTreeMap::new();
        for entry in self.by_path.values() {
            if let Some(v) = entry.tags.get(key) {
                *out.entry(v.as_str()).or_insert(0) += 1;
            }
        }
        out
    }

    /// Applies edits record by record, updating the reverse index for just those records. Returns
    /// how many records were inserted, replaced or removed; [`AssetRegistry::generation`] is bumped
    /// once when that is not zero.
    pub fn apply_changes(&mut self, changes: impl IntoIterator<Item = RegistryChange>) -> usize {
        let mut changed = 0;
        for change in changes {
            changed += match change {
                RegistryChange::Upsert(entry) => usize::from(self.upsert(entry)),
                RegistryChange::RemovePath(path) => usize::from(self.remove_path(&path)),
                RegistryChange::RemoveSource(source) => {
                    let sub = format!("{source}#");
                    // Every path of the source sorts at or after `source` and starts with it.
                    let paths: Vec<String> = self
                        .by_path
                        .range(source.clone()..)
                        .map(|(p, _)| p)
                        .take_while(|p| p.starts_with(source.as_str()))
                        .filter(|p| *p == &source || p.starts_with(&sub))
                        .cloned()
                        .collect();
                    paths.iter().filter(|p| self.remove_path(p)).count()
                }
            };
        }
        if changed > 0 {
            self.generation += 1;
        }
        changed
    }

    /// Replaces a source's records (`source` and `source#...`) with `entries`: the change list the
    /// live cook applies after re-cooking one source.
    pub fn replace_source(&mut self, source: &str, entries: Vec<RegistryEntry>) -> usize {
        let keep: BTreeSet<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        let sub = format!("{source}#");
        let stale: Vec<String> = self
            .by_path
            .keys()
            .filter(|p| (*p == source || p.starts_with(&sub)) && !keep.contains(p.as_str()))
            .cloned()
            .collect();
        let changes = stale
            .into_iter()
            .map(RegistryChange::RemovePath)
            .chain(entries.into_iter().map(RegistryChange::Upsert));
        self.apply_changes(changes)
    }

    /// Merges `other` into `self`; `other`'s records win (later paks override earlier ones).
    pub fn merge(&mut self, other: AssetRegistry) {
        let changes = other.by_path.into_values().map(RegistryChange::Upsert);
        self.apply_changes(changes);
    }

    /// Inserts or replaces; `false` when an identical record was already there.
    fn upsert(&mut self, mut entry: RegistryEntry) -> bool {
        entry.dependencies.sort();
        entry.dependencies.dedup();
        if self.by_path.get(&entry.path) == Some(&entry) {
            return false;
        }
        if let Some(old_path) = self.path_of.get(&entry.id).cloned() {
            self.remove_path(&old_path);
        }
        let path = entry.path.clone();
        self.remove_path(&path);
        for dep in &entry.dependencies {
            self.referencers.entry(*dep).or_default().insert(entry.id);
        }
        self.path_of.insert(entry.id, path.clone());
        self.by_path.insert(path, entry);
        true
    }

    fn remove_path(&mut self, path: &str) -> bool {
        let Some(old) = self.by_path.remove(path) else {
            return false;
        };
        self.path_of.remove(&old.id);
        for dep in &old.dependencies {
            if let Some(set) = self.referencers.get_mut(dep) {
                set.remove(&old.id);
                if set.is_empty() {
                    self.referencers.remove(dep);
                }
            }
        }
        true
    }

    /// Encodes the registry file (magic, version, records sorted by path).
    ///
    /// # Errors
    ///
    /// Postcard encoding failed.
    pub fn to_bytes(&self) -> Result<Vec<u8>, RegistryError> {
        let entries: Vec<&RegistryEntry> = self.by_path.values().collect();
        let body = postcard::to_allocvec(&entries)?;
        let mut out = Vec::with_capacity(12 + body.len());
        out.extend_from_slice(&REGISTRY_MAGIC);
        out.extend_from_slice(&REGISTRY_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Decodes a registry file. Type paths written before the rename to Regia Engine are spelled
    /// with the current crate names (as `AssetManifest::from_postcard` does).
    ///
    /// # Errors
    ///
    /// Bad magic, a newer version, or a record list that does not decode.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RegistryError> {
        if bytes.len() < 12 || bytes[..8] != REGISTRY_MAGIC {
            return Err(RegistryError::BadMagic);
        }
        let mut version = [0u8; 4];
        version.copy_from_slice(&bytes[8..12]);
        let found = u32::from_le_bytes(version);
        if found == 0 || found > REGISTRY_FORMAT_VERSION {
            return Err(RegistryError::UnsupportedVersion {
                found,
                supported: REGISTRY_FORMAT_VERSION,
            });
        }
        let mut entries: Vec<RegistryEntry> = postcard::from_bytes(&bytes[12..])?;
        for entry in &mut entries {
            if let std::borrow::Cow::Owned(current) = regia_core::brand::current_type_path(&entry.type_path) {
                entry.type_path = current;
            }
        }
        Ok(Self::from_entries(entries))
    }

    /// Writes the registry to `path` atomically (unique sibling temp file, then rename).
    ///
    /// # Errors
    ///
    /// Encoding, the write or the rename failed.
    pub fn write_to(&self, path: &Path) -> Result<(), RegistryError> {
        static NEXT_TMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let bytes = self.to_bytes()?;
        let n = NEXT_TMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = path
            .file_name()
            .map_or_else(|| "registry".into(), |n| n.to_string_lossy().into_owned());
        let tmp = path.with_file_name(format!("{name}.{}-{n}.tmp", std::process::id()));
        if let Err(error) = std::fs::write(&tmp, &bytes).and_then(|()| std::fs::rename(&tmp, path)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error.into());
        }
        Ok(())
    }

    /// [`AssetRegistry::write_to`] unless `path` already holds the same bytes, so an unchanged cook
    /// leaves the file (and its modification time) alone. Returns whether it wrote.
    ///
    /// # Errors
    ///
    /// Encoding, the write or the rename failed.
    pub fn write_if_changed(&self, path: &Path) -> Result<bool, RegistryError> {
        let bytes = self.to_bytes()?;
        if std::fs::read(path).is_ok_and(|old| old == bytes) {
            return Ok(false);
        }
        self.write_to(path)?;
        Ok(true)
    }

    /// Reads a registry file; `Ok(None)` when it does not exist.
    ///
    /// # Errors
    ///
    /// The file exists but cannot be read or decoded.
    pub fn read_from(path: &Path) -> Result<Option<Self>, RegistryError> {
        match std::fs::read(path) {
            Ok(bytes) => Self::from_bytes(&bytes).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

/// Whether a record's type path matches a query type: equal, or the record's path ends with
/// `::` + the query (`CookedTexture` matches `regia_asset::CookedTexture`).
fn type_matches(type_path: &str, query: &str) -> bool {
    type_path == query
        || type_path
            .strip_suffix(query)
            .is_some_and(|head| head.ends_with("::"))
}

/// The registry path in a cooked cache directory.
pub fn cache_registry_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join(REGISTRY_FILE_NAME)
}

/// The registry written next to a pak (`content/base.fpak` → `content/base.registry.bin`).
pub fn pak_registry_path(pak: &Path) -> PathBuf {
    let stem = pak
        .file_stem()
        .map_or_else(|| "pak".into(), |s| s.to_string_lossy().into_owned());
    pak.with_file_name(format!("{stem}{PAK_REGISTRY_SUFFIX}"))
}

/// Size and modification time of a file, to notice a rewrite.
type FileStamp = (u64, Option<std::time::SystemTime>);

fn stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()))
}

/// The registry a running app answers queries from: the cooked cache's registry in dev runs, or
/// the registries next to the mounted paks (merged in name order) in a packaged build. Loaded on
/// first use by the agent methods and re-read when one of its files changes on disk (the live cook
/// rewrites the cache registry after each re-cook).
#[derive(Resource, Clone, Debug)]
pub struct LoadedAssetRegistry {
    /// The merged registry.
    pub registry: AssetRegistry,
    /// The files it was read from.
    pub sources: Vec<PathBuf>,
    stamps: Vec<Option<FileStamp>>,
    /// How many times the registry was (re)loaded.
    pub loads: u64,
}

impl LoadedAssetRegistry {
    /// Loads the registry for a cache directory and an optional pak content directory: every
    /// `*.registry.bin` of `content_dir` (name order, later wins), then the cache registry on top.
    /// `Ok(None)` when no registry file exists.
    ///
    /// # Errors
    ///
    /// A registry file exists but cannot be read or decoded.
    pub fn load(cache_dir: &Path, content_dir: Option<&Path>) -> Result<Option<Self>, RegistryError> {
        let mut files = Vec::new();
        if let Some(dir) = content_dir
            && let Ok(read) = std::fs::read_dir(dir)
        {
            let mut paks: Vec<PathBuf> = read
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.ends_with(PAK_REGISTRY_SUFFIX))
                })
                .collect();
            paks.sort();
            files.extend(paks);
        }
        let cache = cache_registry_path(cache_dir);
        if cache.is_file() {
            files.push(cache);
        }
        if files.is_empty() {
            return Ok(None);
        }
        let mut registry = AssetRegistry::default();
        for file in &files {
            if let Some(part) = AssetRegistry::read_from(file)? {
                registry.merge(part);
            }
        }
        registry.generation = 0;
        let stamps = files.iter().map(|f| stamp(f.as_path())).collect();
        Ok(Some(Self {
            registry,
            sources: files,
            stamps,
            loads: 1,
        }))
    }

    /// True when a source file changed (size or modification time) or disappeared since the load.
    pub fn is_stale(&self) -> bool {
        self.sources
            .iter()
            .zip(&self.stamps)
            .any(|(path, old)| stamp(path) != *old)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> AssetId {
        AssetId(regia_core::id::Guid::from_u128(n))
    }

    fn entry(n: u128, path: &str, deps: &[u128]) -> RegistryEntry {
        RegistryEntry {
            id: id(n),
            path: path.to_string(),
            type_path: "regia_asset::CookedTexture".to_string(),
            dependencies: deps.iter().map(|d| id(*d)).collect(),
            tags: automatic_tags(path),
            size: 10,
        }
    }

    #[test]
    fn type_match_by_suffix_segment() {
        assert!(type_matches("regia_asset::CookedTexture", "CookedTexture"));
        assert!(type_matches(
            "regia_asset::CookedTexture",
            "regia_asset::CookedTexture"
        ));
        assert!(!type_matches("regia_asset::CookedTexture", "Texture"));
    }

    #[test]
    fn upsert_with_a_moved_path_drops_the_old_path() {
        let mut reg = AssetRegistry::from_entries([entry(1, "assets/a.png", &[2])]);
        reg.apply_changes([RegistryChange::Upsert(entry(1, "assets/b.png", &[2]))]);
        assert!(reg.by_path("assets/a.png").is_none());
        assert_eq!(reg.by_id(id(1)).map(|e| e.path.as_str()), Some("assets/b.png"));
        assert_eq!(reg.referencers(id(2), false).entries.len(), 1);
    }

    #[test]
    fn identical_upsert_is_not_a_change() {
        let mut reg = AssetRegistry::from_entries([entry(1, "assets/a.png", &[])]);
        assert_eq!(
            reg.apply_changes([RegistryChange::Upsert(entry(1, "assets/a.png", &[]))]),
            0
        );
        assert_eq!(reg.generation, 0);
    }
}
