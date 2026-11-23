// SPDX-License-Identifier: MPL-2.0
//! The content manifest (`content-manifest.ron`, format `content-manifest` v1), the chunk map
//! (`chunk-map.bin`, v1), the per-device install record (`content-state.ron`, v1), chunk-name
//! rules and the mount-order sort (`docs/specs/40-content-chunks-dlc-asset-packs.md` § 2.3, § 4).

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

thread_local! {
    static CHUNK_ENTRY_CLONES: Cell<u64> = const { Cell::new(0) };
}

fn note_chunk_entry_clone() {
    CHUNK_ENTRY_CLONES.with(|n| n.set(n.get().saturating_add(1)));
}

/// Times [`ChunkEntry`] was cloned on this thread since [`reset_chunk_entry_clones`].
#[must_use]
pub fn chunk_entry_clones() -> u64 {
    CHUNK_ENTRY_CLONES.with(Cell::get)
}

/// Zeroes [`chunk_entry_clones`] on this thread.
pub fn reset_chunk_entry_clones() {
    CHUNK_ENTRY_CLONES.with(|n| n.set(0));
}

use serde::{Deserialize, Serialize};

use super::ContentError;
use crate::id::AssetId;

/// Version of `content-manifest.ron` this build reads and writes.
pub const CONTENT_MANIFEST_FORMAT_VERSION: u32 = 1;
/// File name of the content manifest, next to the paks.
pub const CONTENT_MANIFEST_FILE: &str = "content-manifest.ron";
/// Version of `chunk-map.bin`.
pub const CHUNK_MAP_FORMAT_VERSION: u32 = 1;
/// File name of the chunk map, next to the manifest.
pub const CHUNK_MAP_FILE: &str = "chunk-map.bin";
/// First bytes of a chunk map file.
pub const CHUNK_MAP_MAGIC: [u8; 8] = *b"RGCHMAP\0";
/// Version of `content-state.ron` (the per-device install record).
pub const CONTENT_STATE_FORMAT_VERSION: u32 = 1;
/// File name of the install record, in the user data directory.
pub const CONTENT_STATE_FILE: &str = "content-state.ron";
/// Suffix of a DLC's single-chunk manifest entry file (`<chunk>.chunk.ron`, a [`ChunkEntry`]),
/// written by `regia content dlc` next to its pak.
pub const CHUNK_ENTRY_SUFFIX: &str = ".chunk.ron";
/// Suffix of a DLC's own chunk map (`<chunk>.chunk-map.bin`, the [`ChunkMap`] format of
/// [`CHUNK_MAP_FILE`] listing only that chunk's ids), written next to its pak.
pub const DLC_CHUNK_MAP_SUFFIX: &str = ".chunk-map.bin";
/// The chunk every build has and every device has installed.
pub const BASE_CHUNK: &str = "base";
/// Longest chunk name.
pub const MAX_CHUNK_NAME_LEN: usize = 40;
/// Extension every chunk file (`ChunkEntry::file`) ends in: a download lands only on a pak, never
/// on the install record, the manifest or a library the host loads (ADR-4922).
pub const CHUNK_FILE_EXTENSION: &str = ".fpak";

/// The serde field-list fingerprint of `content-manifest.ron` (hashed by `cargo xtask check-formats`).
#[must_use]
pub fn content_manifest_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<ContentManifest>(),
        regia_core::schema::serde_field_names::<ChunkEntry>(),
    ])
}

/// `content-manifest.ron` has no migration steps yet (v1).
#[must_use]
pub fn content_manifest_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde field-list fingerprint of `chunk-map.bin` and `content-state.ron`.
#[must_use]
pub fn chunk_map_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<ChunkMap>(),
        regia_core::schema::serde_field_names::<ContentState>(),
        regia_core::schema::serde_field_names::<InstalledChunk>(),
    ])
}

/// The serde field-list fingerprint of `content-state.ron` alone (format `content-state`, hashed by
/// `cargo xtask check-formats`; ADR-7236). [`chunk_map_schema_fingerprint`] still includes it, so a
/// shape change of the install record shows on both rows.
#[must_use]
pub fn content_state_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<ContentState>(),
        regia_core::schema::serde_field_names::<InstalledChunk>(),
    ])
}

/// `content-state.ron` has no migration steps yet (v1). It is a cache: a record the build cannot
/// read is dropped and everything re-verifies.
#[must_use]
pub fn content_state_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// How a chunk reaches a device.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Delivery {
    /// Inside the base package.
    #[default]
    Install,
    /// The store fetches it right after install (Android).
    FastFollow,
    /// The game requests it.
    OnDemand,
    /// Fetched from a source such as HTTP, or shipped as separate DLC files.
    External,
}

impl Delivery {
    /// Parses the `Regia.toml` spelling (`install`, `fast_follow`, `on_demand`, `external`).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "install" => Some(Self::Install),
            "fast_follow" => Some(Self::FastFollow),
            "on_demand" => Some(Self::OnDemand),
            "external" => Some(Self::External),
            _ => None,
        }
    }

    /// The `Regia.toml` / agent spelling.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::FastFollow => "fast_follow",
            Self::OnDemand => "on_demand",
            Self::External => "external",
        }
    }
}

/// One chunk of a build.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkEntry {
    /// Chunk name ([`validate_chunk_name`]).
    pub name: String,
    /// Pak file name (`<chunk>.fpak`), relative to the manifest.
    pub file: String,
    /// File length in bytes.
    pub size: u64,
    /// BLAKE3 of the file, lowercase hex.
    pub hash: String,
    /// How the chunk is delivered.
    pub delivery: Delivery,
    /// Chunks this one may reference (`base` is implicit). A DLC records the base it was cooked
    /// against as `base@<build_id>`.
    #[serde(default)]
    pub requires: Vec<String>,
    /// May replace ids of earlier chunks (patches).
    #[serde(default)]
    pub overrides: bool,
    /// Position in the mount order (base is 0).
    #[serde(default)]
    pub mount_order: u32,
    /// Number of assets in the pak.
    #[serde(default)]
    pub assets: u32,
    /// Optional BLAKE3 hashes of each 1 MiB block, for resumable transfers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<String>,
}

impl Clone for ChunkEntry {
    fn clone(&self) -> Self {
        note_chunk_entry_clone();
        Self {
            name: self.name.clone(),
            file: self.file.clone(),
            size: self.size,
            hash: self.hash.clone(),
            delivery: self.delivery,
            requires: self.requires.clone(),
            overrides: self.overrides,
            mount_order: self.mount_order,
            assets: self.assets,
            blocks: self.blocks.clone(),
        }
    }
}

impl ChunkEntry {
    /// Checks the installed pak against the full-file identity already carried by this entry.
    /// A bounded streaming read avoids allocating the pak or reading indefinitely if it grows.
    pub(crate) fn verify_installed_pak(&self, path: &Path) -> Result<(), ContentError> {
        use std::io::Read as _;
        let _span =
            tracing::info_span!(target: "regia_asset", "content_verify_mount", chunk = %self.name).entered();
        let mismatch = || ContentError::ChunkIntegrityMismatch(self.name.clone());
        let io = |error: std::io::Error| ContentError::Io(format!("{}: {error}", path.display()));
        let meta = std::fs::metadata(path).map_err(io)?;
        if !meta.is_file() || meta.len() != self.size {
            return Err(mismatch());
        }
        let file = std::fs::File::open(path).map_err(io)?;
        let meta = file.metadata().map_err(io)?;
        if !meta.is_file() || meta.len() != self.size {
            return Err(mismatch());
        }
        let mut hasher = blake3::Hasher::new();
        let read = std::io::copy(&mut file.take(self.size.saturating_add(1)), &mut hasher).map_err(io)?;
        if read != self.size || hasher.finalize().to_hex().as_str() != self.hash {
            return Err(mismatch());
        }
        Ok(())
    }

    /// The chunk names in `requires`, with any `@build_id` suffix removed.
    pub fn required_chunks(&self) -> impl Iterator<Item = &str> {
        self.requires
            .iter()
            .map(|r| r.split_once('@').map_or(r.as_str(), |(name, _)| name))
    }

    /// The base build id this chunk was cooked against (`base@<build_id>` in `requires`).
    #[must_use]
    pub fn base_build(&self) -> Option<&str> {
        self.requires
            .iter()
            .find_map(|r| r.strip_prefix(BASE_CHUNK).and_then(|rest| rest.strip_prefix('@')))
    }
}

/// `content-manifest.ron`: every chunk of a build.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentManifest {
    /// [`CONTENT_MANIFEST_FORMAT_VERSION`].
    pub format_version: u32,
    /// Build identity (`<version>-<build_number>-<cook hash>`).
    pub build_id: String,
    /// Platform the paks were cooked for.
    pub platform: String,
    /// Other build ids this build accepts as a base for DLC.
    #[serde(default)]
    pub compatible_with: Vec<String>,
    /// The chunks, in mount order.
    #[serde(default)]
    pub chunks: Vec<ChunkEntry>,
}

impl ContentManifest {
    /// An empty manifest at the current version.
    #[must_use]
    pub fn new(build_id: impl Into<String>, platform: impl Into<String>) -> Self {
        Self {
            format_version: CONTENT_MANIFEST_FORMAT_VERSION,
            build_id: build_id.into(),
            platform: platform.into(),
            compatible_with: Vec::new(),
            chunks: Vec::new(),
        }
    }

    /// The chunk named `name` (exact match).
    #[must_use]
    pub fn chunk(&self, name: &str) -> Option<&ChunkEntry> {
        self.chunks.iter().find(|c| c.name == name)
    }

    /// Index of the chunk named `name`.
    #[must_use]
    pub fn chunk_index(&self, name: &str) -> Option<usize> {
        self.chunks.iter().position(|c| c.name == name)
    }

    /// Whether this build accepts `build_id` as its base (equal, or listed in `compatible_with`).
    #[must_use]
    pub fn accepts_base(&self, build_id: &str) -> bool {
        self.build_id == build_id || self.compatible_with.iter().any(|b| b == build_id)
    }

    /// Canonical RON text (starts with `ContentManifest(`).
    ///
    /// # Errors
    /// RON serialization failed.
    pub fn to_ron_string(&self) -> Result<String, ContentError> {
        let config = regia_core::ron_pretty().indentor("    ".to_owned());
        let mut text = ron::ser::to_string_pretty(self, config)
            .map_err(|e| ContentError::Format(format!("content manifest: {e}")))?;
        if let Some(rest) = text.strip_prefix('(') {
            text = format!("ContentManifest({rest}");
        }
        text.push('\n');
        Ok(text)
    }

    /// Parses `content-manifest.ron`. A newer `format_version` is [`ContentError::ManifestVersion`]
    /// (peeked before the full parse, so a newer shape still reports the version).
    ///
    /// # Errors
    /// Newer version, or text that does not parse.
    pub fn from_ron_str(text: &str) -> Result<Self, ContentError> {
        #[derive(Deserialize)]
        struct Peek {
            #[serde(default)]
            format_version: u32,
        }
        #[derive(Deserialize)]
        #[serde(rename = "ContentManifest")]
        struct NamedPeek {
            #[serde(default)]
            format_version: u32,
        }
        let peeked = ron::from_str::<Peek>(text)
            .map(|p| p.format_version)
            .or_else(|_| ron::from_str::<NamedPeek>(text).map(|p| p.format_version));
        if let Ok(found) = peeked
            && found > CONTENT_MANIFEST_FORMAT_VERSION
        {
            return Err(ContentError::ManifestVersion {
                found,
                supported: CONTENT_MANIFEST_FORMAT_VERSION,
            });
        }
        let manifest: Self =
            ron::from_str(text).map_err(|e| ContentError::Format(format!("content manifest: {e}")))?;
        // The parsed version, not only the peek: the format starts at 1, so a zero version word
        // is damage, and the peek may have failed on a shape it did not expect
        // (audit-v035-formats 11).
        if manifest.format_version == 0 || manifest.format_version > CONTENT_MANIFEST_FORMAT_VERSION {
            return Err(ContentError::ManifestVersion {
                found: manifest.format_version,
                supported: CONTENT_MANIFEST_FORMAT_VERSION,
            });
        }
        manifest.validate()?;
        Ok(manifest)
    }

    /// The checks [`ContentManifest::from_ron_str`] runs after parsing: safe build ids, chunk names
    /// and file names, one chunk per file.
    fn validate(&self) -> Result<(), ContentError> {
        let manifest = self;
        // The build id goes into every chunk request line (`<base_url>/<build_id>/<file>`): CR/LF
        // would split it and `/` or `..` would name another CDN path (audit v0.2.12).
        for id in std::iter::once(&manifest.build_id).chain(&manifest.compatible_with) {
            validate_build_id(id).map_err(|why| ContentError::Format(format!("build id {id:?}: {why}")))?;
        }
        for chunk in &manifest.chunks {
            if let Some(base) = chunk.base_build() {
                validate_build_id(base).map_err(|why| {
                    ContentError::Format(format!("chunk {:?}: base build id {base:?}: {why}", chunk.name))
                })?;
            }
            validate_chunk_name(&chunk.name)
                .map_err(|why| ContentError::Format(format!("chunk name {:?}: {why}", chunk.name)))?;
            // `:` too: on Windows `C:x` is drive-relative and replaces the content dir on join
            // (ADR-1816). The shared rules add the empty name, a device name that swallows every
            // write on Windows, and control characters: the name goes into a request line.
            if chunk.file.contains(['/', '\\'])
                || chunk.file.contains("..")
                || chunk.file.contains(|c: char| c.is_ascii_control())
                || regia_core::project_path::check_relative(&chunk.file).is_err()
            {
                return Err(ContentError::Format(format!(
                    "chunk {:?}: file {:?} must be a bare file name",
                    chunk.name, chunk.file
                )));
            }
            // A pak and nothing else (finding 21, ADR-4922): ASCII, so the case-folded comparison
            // below is the one every file system makes.
            let stem = chunk.file.strip_suffix(CHUNK_FILE_EXTENSION).unwrap_or("");
            if stem.is_empty() || !chunk.file.is_ascii() {
                return Err(ContentError::Format(format!(
                    "chunk {:?}: file {:?} must be an ASCII `<name>{CHUNK_FILE_EXTENSION}`",
                    chunk.name, chunk.file
                )));
            }
        }
        // Two chunks on one file would share its `.part` and its install path; Windows and the
        // default macOS volume fold case, so `A.fpak` and `a.fpak` are one file there.
        let mut files = std::collections::BTreeSet::new();
        for chunk in &manifest.chunks {
            if !files.insert(chunk.file.to_ascii_lowercase()) {
                return Err(ContentError::Format(format!(
                    "chunk {:?}: file {:?} belongs to another chunk",
                    chunk.name, chunk.file
                )));
            }
        }
        // And one entry per name: a DLC entry may not shadow a listed chunk.
        let mut names = std::collections::BTreeSet::new();
        for chunk in &manifest.chunks {
            if !names.insert(chunk.name.as_str()) {
                return Err(ContentError::Format(format!(
                    "chunk {:?} is listed twice",
                    chunk.name
                )));
            }
        }
        Ok(())
    }

    /// [`ContentManifest::read_from_dir`] plus the DLC entries next to it
    /// ([`ContentManifest::add_dlc_entries`]): what the asset server and the content manager read
    /// (spec 40 § 2.4: a DLC needs only its pak and its entry file).
    ///
    /// # Errors
    /// As [`ContentManifest::read_from_dir`]; a bad DLC entry is skipped with a warning instead.
    pub fn read_from_dir_with_dlc(dir: &Path) -> Result<Option<Self>, ContentError> {
        let Some(mut manifest) = Self::read_from_dir(dir)? else {
            return Ok(None);
        };
        for warning in manifest.add_dlc_entries(dir) {
            tracing::warn!(target: "regia_asset", "DLC entry skipped: {warning}");
        }
        Ok(Some(manifest))
    }

    /// Adds every `<chunk>.chunk.ron` ([`CHUNK_ENTRY_SUFFIX`]) in `dir` to this manifest, in
    /// file-name order, and returns why any was skipped. An entry is skipped when it does not parse,
    /// its name is not its file's stem, it names a chunk the manifest already lists, it records no
    /// `base@<build id>`, or it fails the manifest's own checks (names, bare file, build ids). An
    /// entry whose base this build does not accept is added: mounting it is
    /// [`ContentError::ChunkBaseMismatch`] (the game asks the player to update).
    pub fn add_dlc_entries(&mut self, dir: &Path) -> Vec<String> {
        let mut skipped = Vec::new();
        let Ok(read) = std::fs::read_dir(dir) else {
            return skipped;
        };
        let mut files: Vec<(String, std::path::PathBuf)> = read
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = e.file_name().to_str()?.to_owned();
                name.ends_with(CHUNK_ENTRY_SUFFIX).then(|| (name, e.path()))
            })
            .collect();
        files.sort();
        for (file_name, path) in files {
            let stem = file_name.strip_suffix(CHUNK_ENTRY_SUFFIX).unwrap_or(&file_name);
            let entry = match std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|text| ron::from_str::<ChunkEntry>(&text).map_err(|e| e.to_string()))
            {
                Ok(entry) => entry,
                Err(why) => {
                    skipped.push(format!("{file_name}: {why}"));
                    continue;
                }
            };
            if entry.name != stem {
                skipped.push(format!("{file_name}: names chunk {:?}, not {stem:?}", entry.name));
                continue;
            }
            if self.chunk(&entry.name).is_some() {
                skipped.push(format!("{file_name}: chunk {:?} is already listed", entry.name));
                continue;
            }
            if entry.base_build().is_none() {
                skipped.push(format!("{file_name}: records no `{BASE_CHUNK}@<build id>`"));
                continue;
            }
            let mut candidate = self.clone();
            candidate.chunks.push(entry);
            if let Err(why) = candidate.validate() {
                skipped.push(format!("{file_name}: {why}"));
                continue;
            }
            *self = candidate;
        }
        skipped
    }

    /// Reads `<dir>/content-manifest.ron`; `Ok(None)` when the file does not exist.
    ///
    /// # Errors
    /// IO error other than not-found, newer version, or bad text.
    pub fn read_from_dir(dir: &Path) -> Result<Option<Self>, ContentError> {
        match std::fs::read_to_string(dir.join(CONTENT_MANIFEST_FILE)) {
            Ok(text) => Self::from_ron_str(&text).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ContentError::Io(e.to_string())),
        }
    }

    /// Writes `<dir>/content-manifest.ron` atomically.
    ///
    /// # Errors
    /// Serialization or IO error.
    pub fn write_to_dir(&self, dir: &Path) -> Result<(), ContentError> {
        write_atomic(&dir.join(CONTENT_MANIFEST_FILE), self.to_ron_string()?.as_bytes())
    }
}

/// Checks a chunk name: lowercase ASCII letters, digits, `_` and `-`, starting with a letter, at
/// most [`MAX_CHUNK_NAME_LEN`] characters. Returns why it is invalid.
///
/// # Errors
/// The reason the name is invalid.
pub fn validate_chunk_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("is empty");
    }
    if name.len() > MAX_CHUNK_NAME_LEN {
        return Err("is longer than 40 characters");
    }
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        return Err("must start with a lowercase letter");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err("may hold only lowercase letters, digits, `_` and `-`");
    }
    // `<name>.fpak` must be a file on Windows too: `nul.fpak` writes to the device.
    if regia_core::project_path::check_relative(name).is_err() {
        return Err("is a Windows device name");
    }
    Ok(())
}

/// Longest build id accepted ([`validate_build_id`]).
pub const MAX_BUILD_ID_LEN: usize = 128;

/// Checks a build id: it is one path segment of every chunk request (`<base_url>/<build_id>/<file>`),
/// so it must be non-empty, at most [`MAX_BUILD_ID_LEN`] bytes, and free of `/`, `\`, spaces, ASCII
/// control characters (CR/LF would split the request line) and `..`. Returns why it is invalid.
///
/// # Errors
/// The reason the id is invalid.
pub fn validate_build_id(id: &str) -> Result<(), &'static str> {
    if id.is_empty() {
        return Err("is empty");
    }
    if id.len() > MAX_BUILD_ID_LEN {
        return Err("is longer than 128 bytes");
    }
    if id.contains(['/', '\\', ' ']) || id.contains(|c: char| c.is_ascii_control()) {
        return Err("must not hold `/`, `\\`, spaces or control characters");
    }
    if id.contains("..") {
        return Err("must not hold `..`");
    }
    Ok(())
}

/// The Android asset-pack name of a chunk: `-` replaced by `_`.
#[must_use]
pub fn pack_name(chunk: &str) -> String {
    chunk.replace('-', "_")
}

/// Mount positions from `requires`: a topological sort with `base` first and ties broken by
/// chunk name. `chunks` maps each chunk to the chunks it requires (`base` implicit, unknown names
/// ignored here: [`ContentError::ChunkNotFound`] is the caller's check). Returns each chunk's
/// position (0 for base).
///
/// # Errors
/// [`ContentError::ChunkCycle`] naming the chunks left in a cycle.
pub fn mount_order(chunks: &BTreeMap<String, Vec<String>>) -> Result<BTreeMap<String, u32>, ContentError> {
    let names: BTreeSet<&str> = chunks.keys().map(String::as_str).collect();
    let mut pending: BTreeMap<&str, BTreeSet<&str>> = chunks
        .iter()
        .map(|(name, reqs)| {
            let deps = reqs
                .iter()
                .map(|r| r.split_once('@').map_or(r.as_str(), |(n, _)| n))
                .filter(|r| *r != name && names.contains(r))
                .collect();
            (name.as_str(), deps)
        })
        .collect();
    let mut order = BTreeMap::new();
    let mut next = 0u32;
    if pending.remove(BASE_CHUNK).is_some() {
        order.insert(BASE_CHUNK.to_owned(), next);
        next += 1;
        for deps in pending.values_mut() {
            deps.remove(BASE_CHUNK);
        }
    }
    while !pending.is_empty() {
        // BTreeMap iteration: the first ready chunk is the smallest name.
        let Some(ready) = pending.iter().find(|(_, deps)| deps.is_empty()).map(|(n, _)| *n) else {
            return Err(ContentError::ChunkCycle(
                pending.keys().map(|s| (*s).to_owned()).collect(),
            ));
        };
        pending.remove(ready);
        for deps in pending.values_mut() {
            deps.remove(ready);
        }
        order.insert(ready.to_owned(), next);
        next += 1;
    }
    Ok(order)
}

/// `chunk-map.bin`: which chunk each cooked asset lives in, so the runtime answers `chunk_of`
/// without loading a registry. Layout: [`CHUNK_MAP_MAGIC`], the version as little-endian `u32`,
/// then postcard of the chunk names and the `(asset id, chunk index)` pairs sorted by id.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkMap {
    /// Chunk names; the pairs index into this list.
    pub chunks: Vec<String>,
    /// `(asset id, chunk index)`, sorted by id, one pair per id.
    pub entries: Vec<(AssetId, u32)>,
}

impl ChunkMap {
    /// Builds a map from an id → chunk-name assignment.
    #[must_use]
    pub fn from_assignment(chunk_of: &BTreeMap<AssetId, String>) -> Self {
        let chunks: Vec<String> = chunk_of
            .values()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let entries = chunk_of
            .iter()
            .filter_map(|(id, name)| {
                chunks
                    .binary_search(name)
                    .ok()
                    .and_then(|i| u32::try_from(i).ok())
                    .map(|i| (*id, i))
            })
            .collect();
        Self { chunks, entries }
    }

    /// The chunk of `id`.
    #[must_use]
    pub fn chunk_of(&self, id: AssetId) -> Option<&str> {
        let at = self.entries.binary_search_by_key(&id, |(i, _)| *i).ok()?;
        let index = usize::try_from(self.entries[at].1).ok()?;
        self.chunks.get(index).map(String::as_str)
    }

    /// Ids in chunk `name`, sorted.
    #[must_use]
    pub fn ids_in(&self, name: &str) -> Vec<AssetId> {
        let Some(index) = self.chunks.iter().position(|c| c == name) else {
            return Vec::new();
        };
        self.entries
            .iter()
            .filter(|(_, c)| usize::try_from(*c).ok() == Some(index))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Encodes the file bytes.
    ///
    /// # Errors
    /// Postcard encoding failed.
    pub fn to_bytes(&self) -> Result<Vec<u8>, ContentError> {
        let mut bytes = Vec::with_capacity(12 + self.entries.len() * 20);
        bytes.extend_from_slice(&CHUNK_MAP_MAGIC);
        bytes.extend_from_slice(&CHUNK_MAP_FORMAT_VERSION.to_le_bytes());
        let body =
            postcard::to_allocvec(self).map_err(|e| ContentError::Format(format!("chunk map: {e}")))?;
        bytes.extend_from_slice(&body);
        Ok(bytes)
    }

    /// Decodes file bytes; a newer version is [`ContentError::ManifestVersion`].
    ///
    /// # Errors
    /// Bad magic, newer version, or a body that does not decode.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ContentError> {
        let Some((magic, rest)) = bytes.split_first_chunk::<8>() else {
            return Err(ContentError::Format("chunk map: truncated".into()));
        };
        if *magic != CHUNK_MAP_MAGIC {
            return Err(ContentError::Format("chunk map: bad magic".into()));
        }
        let Some((version, body)) = rest.split_first_chunk::<4>() else {
            return Err(ContentError::Format("chunk map: truncated".into()));
        };
        let found = u32::from_le_bytes(*version);
        // The format starts at 1: a zero version word is damage, not a current file.
        if found == 0 || found > CHUNK_MAP_FORMAT_VERSION {
            return Err(ContentError::ManifestVersion {
                found,
                supported: CHUNK_MAP_FORMAT_VERSION,
            });
        }
        let mut map: Self =
            postcard::from_bytes(body).map_err(|e| ContentError::Format(format!("chunk map: {e}")))?;
        map.entries.sort_by_key(|(id, _)| *id);
        map.entries.dedup_by_key(|(id, _)| *id);
        Ok(map)
    }

    /// Maps every id of `other` to its chunk there, replacing this map's chunk for an id both
    /// list (a DLC that overrides a base asset is mounted after it and wins).
    pub fn merge(&mut self, other: &ChunkMap) {
        let mut merged: BTreeMap<AssetId, String> = self
            .entries
            .iter()
            .filter_map(|(id, c)| Some((*id, self.chunks.get(usize::try_from(*c).ok()?)?.clone())))
            .collect();
        for (id, c) in &other.entries {
            if let Some(name) = usize::try_from(*c).ok().and_then(|i| other.chunks.get(i)) {
                merged.insert(*id, name.clone());
            }
        }
        *self = Self::from_assignment(&merged);
    }

    /// [`ChunkMap::read_from_dir`] (empty when absent) with the own map of each DLC chunk
    /// `manifest` lists ([`DLC_CHUNK_MAP_SUFFIX`], merged in manifest order). A DLC map that does
    /// not read, or that names another chunk, is skipped with a warning.
    ///
    /// # Errors
    /// The base map does not read.
    pub fn read_from_dir_with_dlc(dir: &Path, manifest: &ContentManifest) -> Result<Self, ContentError> {
        let mut map = Self::read_from_dir(dir)?.unwrap_or_default();
        for chunk in manifest.chunks.iter().filter(|c| c.base_build().is_some()) {
            let file = dir.join(format!("{}{DLC_CHUNK_MAP_SUFFIX}", chunk.name));
            let dlc = match std::fs::read(&file) {
                Ok(bytes) => Self::from_bytes(&bytes),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => Err(ContentError::Io(e.to_string())),
            };
            match dlc {
                Ok(dlc) if dlc.chunks.iter().all(|c| *c == chunk.name) => map.merge(&dlc),
                Ok(_) => {
                    tracing::warn!(target: "regia_asset", file = %file.display(), "DLC chunk map names another chunk; skipped")
                }
                Err(e) => {
                    tracing::warn!(target: "regia_asset", file = %file.display(), "DLC chunk map skipped: {e}")
                }
            }
        }
        Ok(map)
    }

    /// Reads `<dir>/chunk-map.bin`; `Ok(None)` when absent.
    ///
    /// # Errors
    /// IO error other than not-found, or bad bytes.
    pub fn read_from_dir(dir: &Path) -> Result<Option<Self>, ContentError> {
        match std::fs::read(dir.join(CHUNK_MAP_FILE)) {
            Ok(bytes) => Self::from_bytes(&bytes).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ContentError::Io(e.to_string())),
        }
    }

    /// Writes `<dir>/chunk-map.bin` atomically.
    ///
    /// # Errors
    /// Encoding or IO error.
    pub fn write_to_dir(&self, dir: &Path) -> Result<(), ContentError> {
        write_atomic(&dir.join(CHUNK_MAP_FILE), &self.to_bytes()?)
    }
}

/// One installed chunk in `content-state.ron`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledChunk {
    /// Build id the file was installed for.
    pub build_id: String,
    /// BLAKE3 of the installed file.
    pub hash: String,
    /// File length when verified (with `modified`, skips re-verification).
    #[serde(default)]
    pub size: u64,
    /// Modification time in seconds since the Unix epoch when verified.
    #[serde(default)]
    pub modified: u64,
}

/// `content-state.ron`: the per-device install record. A cache; deleting it is always safe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentState {
    /// [`CONTENT_STATE_FORMAT_VERSION`].
    pub format_version: u32,
    /// Installed chunks by name.
    #[serde(default)]
    pub chunks: BTreeMap<String, InstalledChunk>,
}

impl Default for ContentState {
    fn default() -> Self {
        Self {
            format_version: CONTENT_STATE_FORMAT_VERSION,
            chunks: BTreeMap::new(),
        }
    }
}

impl ContentState {
    /// Reads `<dir>/content-state.ron`. A missing, unreadable or newer file gives an empty
    /// record (everything re-verifies), never an error.
    #[must_use]
    pub fn read_from_dir(dir: &Path) -> Self {
        std::fs::read_to_string(dir.join(CONTENT_STATE_FILE))
            .ok()
            .and_then(|text| Self::from_ron_str(&text).ok())
            .unwrap_or_default()
    }

    /// Parses the text of a `content-state.ron` (what [`ContentState::read_from_dir`] reads; the
    /// fuzz target and the format corpus call it).
    ///
    /// # Errors
    /// The text does not parse, or its `format_version` is newer than
    /// [`CONTENT_STATE_FORMAT_VERSION`] (`manifest_version`).
    pub fn from_ron_str(text: &str) -> Result<Self, ContentError> {
        let state: Self =
            ron::from_str(text).map_err(|e| ContentError::Format(format!("content state: {e}")))?;
        if state.format_version > CONTENT_STATE_FORMAT_VERSION {
            return Err(ContentError::ManifestVersion {
                found: state.format_version,
                supported: CONTENT_STATE_FORMAT_VERSION,
            });
        }
        Ok(state)
    }

    /// The RON text [`ContentState::write_to_dir`] writes.
    ///
    /// # Errors
    /// Serialization error.
    pub fn to_ron_string(&self) -> Result<String, ContentError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| ContentError::Format(format!("content state: {e}")))
    }

    /// Writes `<dir>/content-state.ron` atomically.
    ///
    /// # Errors
    /// Serialization or IO error.
    pub fn write_to_dir(&self, dir: &Path) -> Result<(), ContentError> {
        write_atomic(&dir.join(CONTENT_STATE_FILE), self.to_ron_string()?.as_bytes())
    }
}

/// BLAKE3 of a file as lowercase hex, streamed.
///
/// # Errors
/// The file does not open or read.
pub fn hash_file(path: &Path) -> std::io::Result<String> {
    let _span = tracing::info_span!(target: "regia_asset", "content_verify").entered();
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(std::fs::File::open(path)?)?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// Writes `bytes` to `path` through a sibling temp file and a rename.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ContentError> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| ContentError::Io(e.to_string()))?;
    }
    let name = path
        .file_name()
        .map_or_else(|| "content".into(), |n| n.to_string_lossy());
    let tmp = path.with_file_name(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        // Make the rename durable too (audit v0.2.12): power lost right after a content update
        // could otherwise bring back the old manifest next to replaced chunk files.
        #[cfg(unix)]
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
            && let Ok(dir) = std::fs::File::open(parent)
        {
            let _ = dir.sync_all();
        }
        Ok::<(), std::io::Error>(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.map_err(|e| ContentError::Io(e.to_string()))
}
