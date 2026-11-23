// SPDX-License-Identifier: MPL-2.0
//! Kit folders: the vehicle, prop and character kits Tincal exports for outside modders
//! (`book/user-guide/open-kit.md`, `book/agents/open-kit.md`).
//!
//! A kit is one flat folder named after its `kit_id`:
//!
//! ```text
//! <kit_id>/
//!   kit.json                        the manifest (KitManifest)
//!   <kit_id>.glb                    the model: LOD0 to LOD3, sockets, clips
//!   cockpit_<faction>_<class>.glb   optional: the cockpit set
//!   rig.json  sockets.json          optional: Tincal's rig and socket sidecars
//!   SHA256SUMS                      optional: `sha256sum` lines for the files above
//! ```
//!
//! [`open_kit`] is pure CPU and never panics on the folder's contents. It reads only regular
//! files inside the folder. A flat name (`check_flat_name`: the model and sidecars) refuses a
//! separator or `..`. A `SHA256SUMS` entry (`check_listed_name`) may use `/` inside the folder
//! (`textures/x.png`); `..`, `\`, `:`, and NUL are still refused. A symlink that
//! leaves the folder is refused, and every read is capped ([`MAX_MANIFEST_BYTES`],
//! [`MAX_SIDECAR_BYTES`], [`MAX_GLB_BYTES`]). A kit without `kit.json` or its model, or with a
//! model that is not a self-contained GLB, is a [`KitError`]. Everything else a modder can get
//! wrong (an unknown faction, a bad `rig.json`, a checksum mismatch, a missing cockpit socket) is
//! a [`KitWarning`] on the opened [`Kit`], so the editor can still show what it has.
//!
//! The model is inspected from its GLB header and JSON chunk only ([`inspect_glb`]): node names,
//! `socket_*` nodes with their world transforms, skins, clips and their durations, authored
//! `_LOD<n>` mesh nodes, and the `{"tincal_kit": 1}` extras on a skin's skeleton root that marks
//! a character kit (read when present, never required). A GLB that points at outside files
//! (a buffer or image `uri` that is not a `data:` URI) is refused, so loading its meshes later
//! never reads outside the folder either.
//!
//! SHA256SUMS is checked with the FIPS 180-4 SHA-256 in [`Sha256`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use glam::{Mat4, Quat, Vec3};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The manifest's file name.
pub const MANIFEST_FILE: &str = "kit.json";
/// The rig sidecar's file name.
pub const RIG_FILE: &str = "rig.json";
/// The socket sidecar's file name.
pub const SOCKETS_FILE: &str = "sockets.json";
/// The checksum list's file name.
pub const SUMS_FILE: &str = "SHA256SUMS";
/// The model node a cockpit attaches to.
pub const COCKPIT_SOCKET: &str = "socket_cockpit";

/// Largest `kit.json` read, bytes.
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
/// Largest `rig.json`, `sockets.json` or `SHA256SUMS` read, bytes.
pub const MAX_SIDECAR_BYTES: u64 = 4 * 1024 * 1024;
/// Largest GLB opened or hashed, bytes.
pub const MAX_GLB_BYTES: u64 = 1024 * 1024 * 1024;
/// Largest GLB JSON chunk parsed, bytes.
pub const MAX_GLB_JSON_BYTES: u64 = 64 * 1024 * 1024;
/// Most entries read from a folder listing, `SHA256SUMS`, `rig.json` or `sockets.json`.
pub const MAX_ENTRIES: usize = 4096;
/// Most glTF nodes inspected.
pub const MAX_NODES: usize = 1 << 16;

/// The factions a kit may belong to.
pub const FACTIONS: [&str; 5] = ["rem", "a52", "tru", "jmc", "generic"];
/// The kit kinds the kit-test loader knows.
pub const KINDS: [&str; 5] = ["hull", "prop", "structure", "pilot", "weapon"];
/// The unit classes the kit-test loader knows.
pub const CLASSES: [&str; 9] = [
    "light",
    "battle",
    "heavy",
    "walker",
    "utility",
    "apc",
    "producer",
    "turret",
    "structure",
];

// ---------------------------------------------------------------------------------------------
// Errors and warnings
// ---------------------------------------------------------------------------------------------

/// Why a kit folder could not be opened.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KitError {
    /// The path is not a folder.
    #[error("{0} is not a folder")]
    NotAFolder(PathBuf),
    /// The folder has no `kit.json`.
    #[error("kit.json is missing: a kit folder starts with its manifest")]
    MissingManifest,
    /// `kit.json` is not valid JSON or lacks a required field.
    #[error("kit.json: {0}")]
    Manifest(String),
    /// `kit_id` cannot name a file in the folder.
    #[error(
        "kit.json: kit_id `{0}` is not a usable file name (letters, digits, `_`, `-` and `.`, \
         at most 128 characters, not starting with `.`)"
    )]
    InvalidKitId(String),
    /// `<kit_id>.glb` is not in the folder.
    #[error("{0} is missing: every kit has a <kit_id>.glb model")]
    MissingModel(String),
    /// A file name or link would read outside the folder, or names something other than a file.
    #[error("{file}: {reason}")]
    UnsafePath {
        /// The name as listed.
        file: String,
        /// What is wrong with it.
        reason: String,
    },
    /// A file is over its size cap.
    #[error("{file} is {size} bytes, over the {limit}-byte limit")]
    TooLarge {
        /// The file's name.
        file: String,
        /// Its size (or the size its header claims).
        size: u64,
        /// The cap.
        limit: u64,
    },
    /// The file could not be read.
    #[error("{file}: {message}")]
    Io {
        /// The file's name.
        file: String,
        /// The OS error.
        message: String,
    },
    /// The model is not a GLB the editor can open.
    #[error("{file} is not a usable GLB: {reason}")]
    BadModel {
        /// The file's name.
        file: String,
        /// What is wrong with it.
        reason: String,
    },
}

impl KitError {
    /// A stable snake_case code for the agent protocol and tests.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotAFolder(_) => "not_a_folder",
            Self::MissingManifest => "missing_manifest",
            Self::Manifest(_) => "bad_manifest",
            Self::InvalidKitId(_) => "invalid_kit_id",
            Self::MissingModel(_) => "missing_model",
            Self::UnsafePath { .. } => "unsafe_path",
            Self::TooLarge { .. } => "too_large",
            Self::Io { .. } => "io",
            Self::BadModel { .. } => "bad_model",
        }
    }
}

/// A problem that does not stop the kit from opening.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KitWarning {
    /// The file it is about (`kit.json`, `rig.json`, a model, ...).
    pub file: String,
    /// What is wrong, in a sentence.
    pub message: String,
}

impl KitWarning {
    /// A warning about `file`.
    #[must_use]
    pub fn new(file: &str, message: impl Into<String>) -> Self {
        Self {
            file: file.to_string(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for KitWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.file, self.message)
    }
}

// ---------------------------------------------------------------------------------------------
// Safe file access
// ---------------------------------------------------------------------------------------------

/// A kit folder: resolves flat file names inside it and reads them with caps.
#[derive(Debug, Clone)]
pub struct KitFolder {
    root: PathBuf,
}

impl KitFolder {
    /// The folder at `dir` (canonicalized).
    ///
    /// # Errors
    ///
    /// [`KitError::NotAFolder`] when `dir` is not a readable directory.
    pub fn new(dir: &Path) -> Result<Self, KitError> {
        let is_dir = std::fs::metadata(dir).is_ok_and(|meta| meta.is_dir());
        if !is_dir {
            return Err(KitError::NotAFolder(dir.to_path_buf()));
        }
        let root = dir
            .canonicalize()
            .map_err(|_| KitError::NotAFolder(dir.to_path_buf()))?;
        Ok(Self { root })
    }

    /// The canonical folder path.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves a flat file name in the folder: `Ok(None)` when it does not exist, the path
    /// of the regular file otherwise (a symlink is followed only while it stays inside).
    ///
    /// # Errors
    ///
    /// [`KitError::UnsafePath`] for a name with a separator, `..`, a drive or root, for a
    /// symlink that leaves the folder or dangles, and for anything that is not a regular file.
    pub fn resolve(&self, name: &str) -> Result<Option<PathBuf>, KitError> {
        check_flat_name(name)?;
        let path = self.root.join(name);
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(KitError::Io {
                    file: name.to_string(),
                    message: err.to_string(),
                });
            }
        };
        let path = if meta.file_type().is_symlink() {
            let target = path
                .canonicalize()
                .map_err(|_| unsafe_path(name, "the symlink points nowhere"))?;
            if !target.starts_with(&self.root) {
                return Err(unsafe_path(name, "the symlink leaves the kit folder"));
            }
            target
        } else {
            path
        };
        let is_file = std::fs::metadata(&path).is_ok_and(|meta| meta.is_file());
        if !is_file {
            return Err(unsafe_path(name, "not a regular file"));
        }
        Ok(Some(path))
    }

    /// Resolves a `SHA256SUMS` entry: a flat name or an in-folder subpath such as
    /// `textures/x.png` (forward slashes only). `Ok(None)` when it does not exist.
    ///
    /// # Errors
    ///
    /// [`KitError::UnsafePath`] for `..`, absolute or empty components, backslashes, colons,
    /// NUL, a symlink that leaves the folder or dangles, and anything that is not a regular file.
    pub fn resolve_listed(&self, name: &str) -> Result<Option<PathBuf>, KitError> {
        check_listed_name(name)?;
        let path = self.root.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(None);
            }
            Err(err) => return Err(io_error(name, &err)),
        }
        let target = path
            .canonicalize()
            .map_err(|_| unsafe_path(name, "the symlink points nowhere"))?;
        if !target.starts_with(&self.root) {
            return Err(unsafe_path(name, "the symlink leaves the kit folder"));
        }
        if !std::fs::metadata(&target).is_ok_and(|meta| meta.is_file()) {
            return Err(unsafe_path(name, "not a regular file"));
        }
        Ok(Some(target))
    }

    /// Reads a resolved file whole, refusing it when it is over `limit` bytes.
    ///
    /// # Errors
    ///
    /// [`KitError::TooLarge`] or [`KitError::Io`].
    pub fn read_capped(&self, name: &str, path: &Path, limit: u64) -> Result<Vec<u8>, KitError> {
        let file = open_capped(name, path, limit)?;
        let mut bytes = Vec::new();
        file.take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|err| io_error(name, &err))?;
        if bytes.len() as u64 > limit {
            return Err(KitError::TooLarge {
                file: name.to_string(),
                size: bytes.len() as u64,
                limit,
            });
        }
        Ok(bytes)
    }

    /// The SHA-256 of a resolved file (lowercase hex), streamed, refusing it over `limit`.
    ///
    /// # Errors
    ///
    /// [`KitError::TooLarge`] or [`KitError::Io`].
    pub fn sha256(&self, name: &str, path: &Path, limit: u64) -> Result<String, KitError> {
        let file = open_capped(name, path, limit)?;
        let mut reader = file.take(limit + 1);
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 64 * 1024];
        let mut total = 0u64;
        loop {
            let read = reader.read(&mut buffer).map_err(|err| io_error(name, &err))?;
            if read == 0 {
                break;
            }
            total += read as u64;
            if total > limit {
                return Err(KitError::TooLarge {
                    file: name.to_string(),
                    size: total,
                    limit,
                });
            }
            hasher.update(&buffer[..read]);
        }
        Ok(hex(&hasher.finish()))
    }

    /// The names of the folder's entries, sorted (at most [`MAX_ENTRIES`]).
    #[must_use]
    pub fn entries(&self) -> Vec<String> {
        let Ok(dir) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut names: Vec<String> = dir
            .take(MAX_ENTRIES)
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .collect();
        names.sort();
        names
    }
}

fn unsafe_path(name: &str, reason: &str) -> KitError {
    KitError::UnsafePath {
        file: name.to_string(),
        reason: reason.to_string(),
    }
}

fn io_error(name: &str, err: &std::io::Error) -> KitError {
    KitError::Io {
        file: name.to_string(),
        message: err.to_string(),
    }
}

/// Refuses anything but one plain file-name component.
fn check_flat_name(name: &str) -> Result<(), KitError> {
    if name.is_empty() || name.len() > 255 {
        return Err(unsafe_path(name, "not a file name"));
    }
    if name.contains(['/', '\\', ':', '\0']) {
        return Err(unsafe_path(
            name,
            "kit files are flat: the name must not contain a path",
        ));
    }
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(()),
        _ => Err(unsafe_path(name, "the name leaves the kit folder")),
    }
}

/// Refuses anything but `/`-separated plain components (one or more).
fn check_listed_name(name: &str) -> Result<(), KitError> {
    if name.is_empty() || name.len() > 1024 {
        return Err(unsafe_path(name, "not a file name"));
    }
    if name.contains(['\\', ':', '\0']) {
        return Err(unsafe_path(
            name,
            "the name must use `/` only, with no drive, colon or NUL",
        ));
    }
    for part in name.split('/') {
        let plain = !part.is_empty()
            && part.len() <= 255
            && matches!(Path::new(part).components().next(), Some(Component::Normal(_)))
            && Path::new(part).components().count() == 1;
        if !plain {
            return Err(unsafe_path(name, "the name leaves the kit folder"));
        }
    }
    Ok(())
}

fn open_capped(name: &str, path: &Path, limit: u64) -> Result<File, KitError> {
    let file = File::open(path).map_err(|err| io_error(name, &err))?;
    let meta = file.metadata().map_err(|err| io_error(name, &err))?;
    if !meta.is_file() {
        return Err(unsafe_path(name, "not a regular file"));
    }
    if meta.len() > limit {
        return Err(KitError::TooLarge {
            file: name.to_string(),
            size: meta.len(),
            limit,
        });
    }
    Ok(file)
}

// ---------------------------------------------------------------------------------------------
// kit.json
// ---------------------------------------------------------------------------------------------

/// `kit.json`: what the kit is and which existing unit it borrows from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KitManifest {
    /// The kit's id; also the folder name and the model's file stem (`jmc_hauler_v3`).
    pub kit_id: String,
    /// `hull`, `prop`, `structure`, `pilot` or `weapon`.
    pub kind: String,
    /// The unit class (`light`, `battle`, `heavy`, `walker`, ...); may be empty.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub class: String,
    /// `rem`, `a52`, `tru`, `jmc` or `generic`.
    pub faction: String,
    /// The stem this kit replaces in the kit-test build; empty for a new unit.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub replaces: String,
    /// The existing stem whose sim stats the kit borrows.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub stats_from: String,
    /// The art contract the kit was made to (`production-plan 2026-10-18`).
    #[serde(default, deserialize_with = "null_as_empty")]
    pub contract: String,
    /// The kit's version.
    pub version: u32,
}

fn null_as_empty<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

const MANIFEST_KEYS: [&str; 8] = [
    "kit_id",
    "kind",
    "class",
    "faction",
    "replaces",
    "stats_from",
    "contract",
    "version",
];

impl KitManifest {
    /// Parses and checks `kit.json`. Unknown keys and values outside the known kinds, classes
    /// and factions are warnings; a missing field or an unusable `kit_id` is an error.
    ///
    /// # Errors
    ///
    /// [`KitError::Manifest`] or [`KitError::InvalidKitId`].
    pub fn parse(bytes: &[u8]) -> Result<(Self, Vec<KitWarning>), KitError> {
        let value: Value =
            serde_json::from_slice(bytes).map_err(|err| KitError::Manifest(err.to_string()))?;
        let object = value
            .as_object()
            .ok_or_else(|| KitError::Manifest("the manifest is not a JSON object".into()))?;
        let mut warnings = Vec::new();
        for key in object.keys() {
            if !MANIFEST_KEYS.contains(&key.as_str()) {
                warnings.push(KitWarning::new(
                    MANIFEST_FILE,
                    format!("unknown key `{key}` ignored"),
                ));
            }
        }
        let manifest: Self =
            serde_json::from_value(value).map_err(|err| KitError::Manifest(err.to_string()))?;
        if !valid_kit_id(&manifest.kit_id) {
            return Err(KitError::InvalidKitId(manifest.kit_id));
        }
        if !KINDS.contains(&manifest.kind.as_str()) {
            warnings.push(KitWarning::new(
                MANIFEST_FILE,
                format!("kind `{}` is not one of {}", manifest.kind, KINDS.join(", ")),
            ));
        }
        if !manifest.class.is_empty() && !CLASSES.contains(&manifest.class.as_str()) {
            warnings.push(KitWarning::new(
                MANIFEST_FILE,
                format!("class `{}` is not one of {}", manifest.class, CLASSES.join(", ")),
            ));
        }
        if !FACTIONS.contains(&manifest.faction.as_str()) {
            warnings.push(KitWarning::new(
                MANIFEST_FILE,
                format!(
                    "faction `{}` is not one of {}",
                    manifest.faction,
                    FACTIONS.join(", ")
                ),
            ));
        }
        if manifest.stats_from.is_empty() {
            warnings.push(KitWarning::new(
                MANIFEST_FILE,
                "stats_from is empty: a kit borrows an existing unit's stats",
            ));
        }
        if manifest.version == 0 {
            warnings.push(KitWarning::new(MANIFEST_FILE, "version 0: versions start at 1"));
        }
        Ok((manifest, warnings))
    }

    /// The model's file name, `<kit_id>.glb`.
    #[must_use]
    pub fn model_file(&self) -> String {
        format!("{}.glb", self.kit_id)
    }

    /// The cockpit's file name, `cockpit_<faction>_<class>.glb` (`None` without a class).
    #[must_use]
    pub fn cockpit_file(&self) -> Option<String> {
        (!self.class.is_empty()).then(|| format!("cockpit_{}_{}.glb", self.faction, self.class))
    }
}

/// Whether `id` can name `<id>.glb` inside the folder.
fn valid_kit_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

// ---------------------------------------------------------------------------------------------
// rig.json and sockets.json
// ---------------------------------------------------------------------------------------------

/// One clip from `rig.json` (Tincal's fixed joint clips: `canopy_open`, `gear_deploy`, ...).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RigClip {
    /// The stem it belongs to.
    pub stem: String,
    /// The clip's name.
    pub name: String,
    /// The clip's length in seconds (`secs`), when given.
    pub secs: Option<f32>,
    /// The joint it moves, when given.
    pub joint: Option<String>,
}

/// Reads the clip list of a `rig.json`: `{"stems": {<stem>: {"clips": [...]}}}`, or the same
/// stems at the top level.
///
/// # Errors
///
/// Not JSON, not an object, or a clip that is not an object with a name.
pub fn parse_rig(bytes: &[u8]) -> Result<Vec<RigClip>, String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|err| err.to_string())?;
    let document = value.as_object().ok_or("the rig is not a JSON object")?;
    let stems = document
        .get("stems")
        .and_then(Value::as_object)
        .unwrap_or(document);
    let mut clips = Vec::new();
    for (stem, entry) in stems {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let Some(list) = entry.get("clips") else {
            continue;
        };
        let list = list
            .as_array()
            .ok_or_else(|| format!("stem `{stem}`: clips is not a list"))?;
        for clip in list {
            let clip = clip
                .as_object()
                .ok_or_else(|| format!("stem `{stem}`: a clip is not an object"))?;
            let name = clip
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| format!("stem `{stem}`: a clip has no name"))?;
            if clips.len() >= MAX_ENTRIES {
                return Err(format!("more than {MAX_ENTRIES} clips"));
            }
            clips.push(RigClip {
                stem: stem.clone(),
                name: name.to_string(),
                secs: clip
                    .get("secs")
                    .and_then(Value::as_f64)
                    .map(|s| s as f32)
                    .filter(|s| s.is_finite() && *s >= 0.0),
                joint: clip.get("joint").and_then(Value::as_str).map(str::to_string),
            });
        }
    }
    Ok(clips)
}

/// One socket from `sockets.json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SocketRecord {
    /// The socket's name.
    pub name: String,
    /// The node it hangs from (`parent` or `node`), when given.
    pub parent: Option<String>,
    /// Its position (`position`, `translation`, `pos_gltf`, or a 16-number `matrix` /
    /// `transform`), when given.
    pub position: Option<[f32; 3]>,
}

/// Reads a `sockets.json` in any of the shapes Tincal writes: `{"sockets": [{name, ...}]}`,
/// `{"sockets": {<name>: {...}}}`, or `{<name>: {...}}` (skipping `format`, `stem`, `notes`).
///
/// # Errors
///
/// Not JSON, not an object, or a socket entry that is not an object.
pub fn parse_sockets(bytes: &[u8]) -> Result<Vec<SocketRecord>, String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|err| err.to_string())?;
    let document = value.as_object().ok_or("the socket list is not a JSON object")?;
    let mut raw: Vec<(Option<String>, &Value)> = Vec::new();
    match document.get("sockets") {
        Some(Value::Array(list)) => raw.extend(list.iter().map(|item| (None, item))),
        Some(Value::Object(map)) => raw.extend(map.iter().map(|(name, item)| (Some(name.clone()), item))),
        Some(_) => return Err("sockets is neither a list nor an object".into()),
        None => raw.extend(
            document
                .iter()
                .filter(|(name, item)| {
                    !matches!(name.as_str(), "format" | "stem" | "notes") && item.is_object()
                })
                .map(|(name, item)| (Some(name.clone()), item)),
        ),
    }
    if raw.len() > MAX_ENTRIES {
        return Err(format!("more than {MAX_ENTRIES} sockets"));
    }
    let mut sockets = Vec::new();
    for (key, item) in raw {
        let item = item.as_object().ok_or("a socket entry is not an object")?;
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or(key)
            .filter(|name| !name.is_empty())
            .ok_or("a socket has no name")?;
        let parent = item
            .get("parent")
            .or_else(|| item.get("node"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let position = ["position", "translation", "pos_gltf"]
            .iter()
            .find_map(|key| item.get(*key).and_then(finite_array::<3>))
            .or_else(|| {
                ["matrix", "transform"]
                    .iter()
                    .find_map(|key| item.get(*key).and_then(finite_array::<16>))
                    .map(|m| [m[12], m[13], m[14]])
            });
        sockets.push(SocketRecord {
            name,
            parent,
            position,
        });
    }
    Ok(sockets)
}

/// `N` finite numbers from a JSON array of exactly `N`.
fn finite_array<const N: usize>(value: &Value) -> Option<[f32; N]> {
    let list = value.as_array().filter(|list| list.len() == N)?;
    let mut out = [0f32; N];
    for (slot, item) in out.iter_mut().zip(list) {
        let v = item.as_f64()? as f32;
        if !v.is_finite() {
            return None;
        }
        *slot = v;
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// SHA256SUMS
// ---------------------------------------------------------------------------------------------

/// One `SHA256SUMS` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SumLine {
    /// The file name as listed (a leading `./` removed).
    pub file: String,
    /// The expected digest, lowercase hex.
    pub expected: String,
}

/// Parses `SHA256SUMS`: GNU lines (`<hex>  <name>` or `<hex> *<name>`) and BSD lines
/// (`SHA256 (<name>) = <hex>`); blank lines and `#` comments are skipped. Returns the entries
/// and the 1-based numbers of lines that did not parse.
#[must_use]
pub fn parse_sha256sums(text: &str) -> (Vec<SumLine>, Vec<usize>) {
    let mut entries = Vec::new();
    let mut bad = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let parsed = if let Some(rest) = line.strip_prefix("SHA256 (") {
            rest.rsplit_once(") = ")
                .map(|(name, digest)| (digest.trim(), name))
        } else {
            line.split_once(' ').map(|(digest, rest)| {
                let name = rest
                    .strip_prefix(' ')
                    .or_else(|| rest.strip_prefix('*'))
                    .unwrap_or(rest);
                (digest, name)
            })
        };
        match parsed {
            Some((digest, name))
                if digest.len() == 64
                    && digest.bytes().all(|b| b.is_ascii_hexdigit())
                    && !name.is_empty() =>
            {
                if entries.len() >= MAX_ENTRIES {
                    bad.push(index + 1);
                    continue;
                }
                entries.push(SumLine {
                    file: name.strip_prefix("./").unwrap_or(name).to_string(),
                    expected: digest.to_ascii_lowercase(),
                });
            }
            _ => bad.push(index + 1),
        }
    }
    (entries, bad)
}

/// One listed file's check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ChecksumStatus {
    /// The digest matches.
    Ok,
    /// The digest differs.
    Mismatch {
        /// The file's actual digest.
        actual: String,
    },
    /// The listed file is not in the folder.
    Missing,
    /// The file was not hashed: an unsafe name or link, over the size cap, or unreadable.
    Refused {
        /// Why.
        reason: String,
    },
}

/// One `SHA256SUMS` entry and its outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChecksumEntry {
    /// The file name as listed.
    pub file: String,
    /// The listed digest.
    pub expected: String,
    /// The outcome.
    #[serde(flatten)]
    pub status: ChecksumStatus,
}

/// The overall `SHA256SUMS` verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumVerdict {
    /// The kit has no `SHA256SUMS`.
    Absent,
    /// Every listed file matches and every line parsed.
    Verified,
    /// A file differs, is missing or was refused, or a line did not parse.
    Failed,
    /// `SHA256SUMS` itself could not be read (unsafe, too large, not text).
    Unreadable,
}

impl ChecksumVerdict {
    /// A short label for the panel.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Absent => "no SHA256SUMS",
            Self::Verified => "verified",
            Self::Failed => "FAILED",
            Self::Unreadable => "unreadable",
        }
    }
}

/// The `SHA256SUMS` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChecksumReport {
    /// The verdict.
    pub verdict: ChecksumVerdict,
    /// Every listed file.
    pub entries: Vec<ChecksumEntry>,
    /// Files in the folder that `SHA256SUMS` does not list (itself excluded).
    pub unlisted: Vec<String>,
    /// 1-based numbers of lines that did not parse.
    pub malformed_lines: Vec<usize>,
    /// Why `SHA256SUMS` could not be read.
    pub error: Option<String>,
}

impl ChecksumReport {
    fn absent() -> Self {
        Self {
            verdict: ChecksumVerdict::Absent,
            entries: Vec::new(),
            unlisted: Vec::new(),
            malformed_lines: Vec::new(),
            error: None,
        }
    }

    /// Entries that did not match.
    pub fn failures(&self) -> impl Iterator<Item = &ChecksumEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.status != ChecksumStatus::Ok)
    }
}

/// Checks the folder's `SHA256SUMS` (absent is fine).
#[must_use]
pub fn verify_checksums(folder: &KitFolder) -> ChecksumReport {
    let path = match folder.resolve(SUMS_FILE) {
        Ok(None) => return ChecksumReport::absent(),
        Ok(Some(path)) => path,
        Err(err) => {
            return ChecksumReport {
                error: Some(err.to_string()),
                verdict: ChecksumVerdict::Unreadable,
                ..ChecksumReport::absent()
            };
        }
    };
    let text = folder
        .read_capped(SUMS_FILE, &path, MAX_SIDECAR_BYTES)
        .and_then(|bytes| {
            String::from_utf8(bytes).map_err(|_| KitError::Io {
                file: SUMS_FILE.into(),
                message: "not UTF-8 text".into(),
            })
        });
    let text = match text {
        Ok(text) => text,
        Err(err) => {
            return ChecksumReport {
                error: Some(err.to_string()),
                verdict: ChecksumVerdict::Unreadable,
                ..ChecksumReport::absent()
            };
        }
    };
    let (lines, malformed_lines) = parse_sha256sums(&text);
    let mut entries = Vec::new();
    for line in &lines {
        let status = match folder.resolve_listed(&line.file) {
            Ok(None) => ChecksumStatus::Missing,
            Ok(Some(path)) => match folder.sha256(&line.file, &path, MAX_GLB_BYTES) {
                Ok(actual) if actual == line.expected => ChecksumStatus::Ok,
                Ok(actual) => ChecksumStatus::Mismatch { actual },
                Err(err) => ChecksumStatus::Refused {
                    reason: err.to_string(),
                },
            },
            Err(err) => ChecksumStatus::Refused {
                reason: err.to_string(),
            },
        };
        entries.push(ChecksumEntry {
            file: line.file.clone(),
            expected: line.expected.clone(),
            status,
        });
    }
    let listed: BTreeSet<&str> = lines.iter().map(|line| line.file.as_str()).collect();
    let unlisted = folder
        .entries()
        .into_iter()
        .filter(|name| name != SUMS_FILE && !listed.contains(name.as_str()))
        .filter(|name| folder.resolve(name).is_ok_and(|path| path.is_some()))
        .collect();
    let ok = !entries.is_empty()
        && malformed_lines.is_empty()
        && entries.iter().all(|entry| entry.status == ChecksumStatus::Ok);
    ChecksumReport {
        verdict: if ok {
            ChecksumVerdict::Verified
        } else {
            ChecksumVerdict::Failed
        },
        entries,
        unlisted,
        malformed_lines,
        error: None,
    }
}

// ---------------------------------------------------------------------------------------------
// GLB inspection
// ---------------------------------------------------------------------------------------------

/// A `socket_*` node of a model.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GlbSocket {
    /// The node's name.
    pub name: String,
    /// The node's index.
    pub node: usize,
    /// The node's world matrix (column-major).
    pub matrix: [f32; 16],
}

impl GlbSocket {
    /// The world position.
    #[must_use]
    pub fn position(&self) -> [f32; 3] {
        [self.matrix[12], self.matrix[13], self.matrix[14]]
    }
}

/// A glTF animation of a model.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GlbClip {
    /// The animation's name (`animation_<index>` when it has none).
    pub name: String,
    /// The latest keyframe time over its samplers, seconds (from the accessors' `max`).
    pub duration: f32,
    /// How many channels it animates.
    pub channels: usize,
}

/// What a GLB holds, from its header and JSON chunk (no buffer reads).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct GlbSummary {
    /// Node names in index order (`node_<index>` when a node has none).
    pub nodes: Vec<String>,
    /// How many nodes carry a mesh.
    pub mesh_nodes: usize,
    /// `socket_*` nodes without a mesh.
    pub sockets: Vec<GlbSocket>,
    /// Animations.
    pub clips: Vec<GlbClip>,
    /// How many skins.
    pub skins: usize,
    /// The first skin's skeleton root node name.
    pub skeleton_root: Option<String>,
    /// A skin's skeleton root carries the extras `{"tincal_kit": 1}`.
    pub tincal_kit_marker: bool,
    /// Authored LOD levels: the `n` of mesh nodes named `..._LOD<n>`, sorted.
    pub lod_levels: Vec<u32>,
}

impl GlbSummary {
    /// The socket called `name`.
    #[must_use]
    pub fn socket(&self, name: &str) -> Option<&GlbSocket> {
        self.sockets.iter().find(|socket| socket.name == name)
    }
}

/// The LOD level of a node named `..._LOD<n>` (case-insensitive `_lod`).
#[must_use]
pub fn lod_suffix(name: &str) -> Option<u32> {
    // Byte offsets: digits and `_lod` are ASCII, so `get` never splits a character it keeps.
    let digits_start = name.rfind(|c: char| !c.is_ascii_digit()).map_or(0, |i| i + 1);
    if digits_start == name.len() || digits_start < 4 {
        return None;
    }
    let tag = name.get(digits_start - 4..digits_start)?;
    if !tag.eq_ignore_ascii_case("_lod") {
        return None;
    }
    name.get(digits_start..)?.parse().ok()
}

const GLB_MAGIC: &[u8; 4] = b"glTF";
const CHUNK_JSON: u32 = 0x4E4F_534A;
const CHUNK_BIN: u32 = 0x004E_4942;

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// How many leading bytes of a GLB [`inspect_glb`] needs: the 12-byte header, the JSON chunk,
/// and the BIN chunk's 8-byte header. `head` must hold at least the first 20 bytes.
///
/// # Errors
///
/// Not a GLB header, or a JSON chunk over [`MAX_GLB_JSON_BYTES`].
pub fn glb_head_len(head: &[u8]) -> Result<u64, String> {
    if head.len() < 20 || &head[0..4] != GLB_MAGIC {
        return Err("not a binary glTF (no `glTF` header)".into());
    }
    let json_len = u64::from(u32_at(head, 12).unwrap_or(0));
    if json_len > MAX_GLB_JSON_BYTES {
        return Err(format!(
            "the JSON chunk claims {json_len} bytes, over the {MAX_GLB_JSON_BYTES}-byte limit"
        ));
    }
    Ok(20 + json_len.next_multiple_of(4) + 8)
}

/// Inspects a GLB from its leading bytes (`head`, see [`glb_head_len`]) and its full size on
/// disk. Checks the header's and the chunks' claimed lengths against the size, refuses outside
/// file references, and reads names, sockets, skins, clips and LOD levels from the JSON.
///
/// # Errors
///
/// A malformed header or chunk, a length claim past the file's end, JSON that does not parse,
/// or a buffer or image that references an outside file.
pub fn inspect_glb(head: &[u8], file_len: u64) -> Result<GlbSummary, String> {
    glb_head_len(head)?;
    let version = u32_at(head, 4).unwrap_or(0);
    if version != 2 {
        return Err(format!(
            "glTF container version {version}; only version 2 is supported"
        ));
    }
    let total = u64::from(u32_at(head, 8).unwrap_or(0));
    if total > file_len {
        return Err(format!(
            "the header claims {total} bytes but the file has {file_len}"
        ));
    }
    let json_len = u32_at(head, 12).unwrap_or(0) as usize;
    if u32_at(head, 16) != Some(CHUNK_JSON) {
        return Err("the first chunk is not JSON".into());
    }
    if 20 + json_len as u64 > total {
        return Err(format!(
            "the JSON chunk claims {json_len} bytes, past the {total}-byte container"
        ));
    }
    let json = head.get(20..20 + json_len).ok_or("the JSON chunk is cut short")?;
    let bin_at = 20 + json_len.next_multiple_of(4);
    if (bin_at as u64) + 8 <= total
        && let (Some(bin_len), Some(kind)) = (u32_at(head, bin_at), u32_at(head, bin_at + 4))
    {
        if kind != CHUNK_BIN {
            return Err("the second chunk is not BIN".into());
        }
        if bin_at as u64 + 8 + u64::from(bin_len) > total {
            return Err(format!(
                "the BIN chunk claims {bin_len} bytes, past the {total}-byte container"
            ));
        }
    }
    let document: Value = serde_json::from_slice(json).map_err(|err| format!("JSON chunk: {err}"))?;
    summarize_gltf(&document)
}

fn summarize_gltf(document: &Value) -> Result<GlbSummary, String> {
    for key in ["buffers", "images"] {
        for (index, item) in array(document, key).iter().enumerate() {
            if let Some(uri) = item.get("uri").and_then(Value::as_str)
                && !uri.starts_with("data:")
            {
                return Err(format!(
                    "{key}[{index}] references the outside file `{uri}`; a kit GLB embeds its data"
                ));
            }
        }
    }
    let nodes = array(document, "nodes");
    if nodes.len() > MAX_NODES {
        return Err(format!("{} nodes, over the {MAX_NODES} limit", nodes.len()));
    }
    let names: Vec<String> = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| {
            node.get("name")
                .and_then(Value::as_str)
                .map_or_else(|| format!("node_{index}"), str::to_string)
        })
        .collect();
    let mut parent: Vec<Option<usize>> = vec![None; nodes.len()];
    for (index, node) in nodes.iter().enumerate() {
        for child in array(node, "children") {
            if let Some(child) = child.as_u64().map(|c| c as usize)
                && child < nodes.len()
                && child != index
                && parent[child].is_none()
            {
                parent[child] = Some(index);
            }
        }
    }
    let world = world_matrices(nodes, &parent);
    let has_mesh = |index: usize| nodes[index].get("mesh").is_some();

    let sockets = (0..nodes.len())
        .filter(|&index| !has_mesh(index) && names[index].starts_with("socket_"))
        .map(|index| GlbSocket {
            name: names[index].clone(),
            node: index,
            matrix: world[index].to_cols_array(),
        })
        .collect();

    let skins = array(document, "skins");
    let mut skeleton_root = None;
    let mut marker = false;
    for skin in skins {
        let joints: Vec<usize> = array(skin, "joints")
            .iter()
            .filter_map(Value::as_u64)
            .map(|j| j as usize)
            .filter(|&j| j < nodes.len())
            .collect();
        let root = skin
            .get("skeleton")
            .and_then(Value::as_u64)
            .map(|r| r as usize)
            .filter(|&r| r < nodes.len())
            .or_else(|| {
                joints
                    .iter()
                    .copied()
                    .find(|&j| parent[j].is_none_or(|p| !joints.contains(&p)))
            });
        let Some(root) = root else {
            continue;
        };
        if skeleton_root.is_none() {
            skeleton_root = Some(names[root].clone());
        }
        let flag = nodes[root].get("extras").and_then(|e| e.get("tincal_kit"));
        if flag.is_some_and(|f| f.as_u64() == Some(1) || f.as_bool() == Some(true)) {
            marker = true;
        }
    }

    let accessors = array(document, "accessors");
    let clips = array(document, "animations")
        .iter()
        .enumerate()
        .map(|(index, animation)| {
            let duration = array(animation, "samplers")
                .iter()
                .filter_map(|sampler| sampler.get("input").and_then(Value::as_u64))
                .filter_map(|input| accessors.get(input as usize))
                .filter_map(|accessor| accessor.get("max").and_then(|m| m.get(0)).and_then(Value::as_f64))
                .map(|max| max as f32)
                .filter(|max| max.is_finite() && *max >= 0.0)
                .fold(0.0f32, f32::max);
            GlbClip {
                name: animation
                    .get("name")
                    .and_then(Value::as_str)
                    .map_or_else(|| format!("animation_{index}"), str::to_string),
                duration,
                channels: array(animation, "channels").len(),
            }
        })
        .collect();

    let lod_levels: BTreeSet<u32> = (0..nodes.len())
        .filter(|&index| has_mesh(index))
        .filter_map(|index| lod_suffix(&names[index]))
        .collect();
    Ok(GlbSummary {
        mesh_nodes: (0..nodes.len()).filter(|&index| has_mesh(index)).count(),
        nodes: names,
        sockets,
        clips,
        skins: skins.len(),
        skeleton_root,
        tincal_kit_marker: marker,
        lod_levels: lod_levels.into_iter().collect(),
    })
}

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// A node's local matrix: `matrix`, else T·R·S; a malformed or non-finite transform is identity.
fn local_matrix(node: &Value) -> Mat4 {
    if let Some(m) = node.get("matrix").and_then(finite_array::<16>) {
        return Mat4::from_cols_array(&m);
    }
    let t = node
        .get("translation")
        .and_then(finite_array::<3>)
        .map_or(Vec3::ZERO, Vec3::from_array);
    let r = node
        .get("rotation")
        .and_then(finite_array::<4>)
        .map(Quat::from_array)
        .filter(|q| q.length_squared() > 1e-12)
        .map_or(Quat::IDENTITY, Quat::normalize);
    let s = node
        .get("scale")
        .and_then(finite_array::<3>)
        .map_or(Vec3::ONE, Vec3::from_array);
    Mat4::from_scale_rotation_translation(s, r, t)
}

/// World matrices over the parent links (cycle-safe: each node is visited once).
fn world_matrices(nodes: &[Value], parent: &[Option<usize>]) -> Vec<Mat4> {
    let mut world = vec![Mat4::IDENTITY; nodes.len()];
    let mut done = vec![false; nodes.len()];
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (index, p) in parent.iter().enumerate() {
        if let Some(p) = p {
            children[*p].push(index);
        }
    }
    let roots: Vec<usize> = (0..nodes.len()).filter(|&i| parent[i].is_none()).collect();
    let mut stack: Vec<(usize, Mat4)> = roots.into_iter().map(|r| (r, Mat4::IDENTITY)).collect();
    while let Some((index, up)) = stack.pop() {
        if done[index] {
            continue;
        }
        done[index] = true;
        world[index] = up * local_matrix(&nodes[index]);
        for &child in &children[index] {
            stack.push((child, world[index]));
        }
    }
    world
}

// ---------------------------------------------------------------------------------------------
// The kit
// ---------------------------------------------------------------------------------------------

/// A model file of the kit.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KitModel {
    /// Its file name.
    pub file: String,
    /// Its path (inside the kit folder).
    pub path: PathBuf,
    /// Its size, bytes.
    pub bytes: u64,
    /// What it holds.
    pub summary: GlbSummary,
}

/// Where a kit clip comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipSource {
    /// A glTF animation in the model (playable in the animation preview when the model is skinned).
    Model,
    /// Only `rig.json` lists it.
    Rig,
}

/// One clip of the kit.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KitClip {
    /// The clip's name.
    pub name: String,
    /// Its length, seconds: the model's keyframes, else `rig.json`'s `secs`.
    pub duration: f32,
    /// Where it comes from.
    pub source: ClipSource,
    /// `rig.json`'s `secs` for it, when listed there.
    pub rig_secs: Option<f32>,
    /// `rig.json`'s joint for it, when listed there.
    pub joint: Option<String>,
}

/// One socket of the kit.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KitSocket {
    /// The socket's name.
    pub name: String,
    /// Its world position: the model's node, else `sockets.json`'s position.
    pub position: Option<[f32; 3]>,
    /// The parent node `sockets.json` names.
    pub parent: Option<String>,
    /// The model has a `socket_*` node with this name.
    pub in_model: bool,
    /// `sockets.json` lists it.
    pub in_sockets_json: bool,
}

/// An opened kit folder.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Kit {
    /// The kit folder (canonical).
    pub root: PathBuf,
    /// `kit.json`.
    pub manifest: KitManifest,
    /// `<kit_id>.glb`.
    pub model: KitModel,
    /// The cockpit model, when the kit has one.
    pub cockpit: Option<KitModel>,
    /// Where the cockpit attaches: the world matrix of the model's `socket_cockpit` node
    /// (`None`: no such node, so the cockpit sits at the origin).
    pub cockpit_mount: Option<[f32; 16]>,
    /// Clips: the model's animations, then the `rig.json` clips the model does not have.
    pub clips: Vec<KitClip>,
    /// Sockets: the model's `socket_*` nodes merged with `sockets.json`.
    pub sockets: Vec<KitSocket>,
    /// `rig.json`'s clips as listed.
    pub rig_clips: Vec<RigClip>,
    /// The `SHA256SUMS` result.
    pub checksums: ChecksumReport,
    /// The `{"tincal_kit": 1}` marker on the skeleton root: a character kit.
    pub character_kit: bool,
    /// Everything that did not stop the kit from opening.
    pub warnings: Vec<KitWarning>,
}

/// Opens a kit folder. See the module docs for what is an error and what is a warning.
///
/// # Errors
///
/// The folder, `kit.json` or `<kit_id>.glb` is missing, unsafe, too large or malformed.
#[tracing::instrument(target = "regia_editor", skip_all, name = "open_kit")]
pub fn open_kit(dir: &Path) -> Result<Kit, KitError> {
    let folder = KitFolder::new(dir)?;
    let manifest_path = folder.resolve(MANIFEST_FILE)?.ok_or(KitError::MissingManifest)?;
    let manifest_bytes = folder.read_capped(MANIFEST_FILE, &manifest_path, MAX_MANIFEST_BYTES)?;
    let (manifest, mut warnings) = KitManifest::parse(&manifest_bytes)?;
    let folder_name = folder
        .root()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if folder_name != manifest.kit_id {
        warnings.push(KitWarning::new(
            MANIFEST_FILE,
            format!(
                "kit_id `{}` differs from the folder name `{folder_name}`",
                manifest.kit_id
            ),
        ));
    }

    let model_file = manifest.model_file();
    let model_path = folder
        .resolve(&model_file)?
        .ok_or_else(|| KitError::MissingModel(model_file.clone()))?;
    let model = open_model(&model_file, &model_path)?;

    let cockpit = find_cockpit(&folder, &manifest, &mut warnings);
    let cockpit_mount = model.summary.socket(COCKPIT_SOCKET).map(|socket| socket.matrix);
    if cockpit.is_some() && cockpit_mount.is_none() {
        warnings.push(KitWarning::new(
            &model_file,
            format!("no {COCKPIT_SOCKET} node: the cockpit is placed at the origin"),
        ));
    }

    let rig_clips = read_sidecar(&folder, RIG_FILE, parse_rig, &mut warnings).unwrap_or_default();
    let socket_records =
        read_sidecar(&folder, SOCKETS_FILE, parse_sockets, &mut warnings).unwrap_or_default();
    let checksums = verify_checksums(&folder);
    checksum_warnings(&checksums, &mut warnings);

    let clips = merge_clips(&model.summary, &rig_clips);
    let sockets = merge_sockets(&model.summary, &socket_records, &mut warnings);
    let character_kit = model.summary.tincal_kit_marker;
    Ok(Kit {
        root: folder.root().to_path_buf(),
        manifest,
        model,
        cockpit,
        cockpit_mount,
        clips,
        sockets,
        rig_clips,
        checksums,
        character_kit,
        warnings,
    })
}

/// Opens and inspects one GLB of the kit (reads only its head).
fn open_model(name: &str, path: &Path) -> Result<KitModel, KitError> {
    let file = open_capped(name, path, MAX_GLB_BYTES)?;
    let bytes = file.metadata().map_err(|err| io_error(name, &err))?.len();
    let bad = |reason: String| KitError::BadModel {
        file: name.to_string(),
        reason,
    };
    let mut reader = file.take(MAX_GLB_BYTES);
    let mut head = vec![0u8; 20.min(bytes as usize)];
    reader.read_exact(&mut head).map_err(|err| io_error(name, &err))?;
    let want = glb_head_len(&head).map_err(bad)?.min(bytes);
    let mut rest = Vec::new();
    (&mut reader)
        .take(want.saturating_sub(head.len() as u64))
        .read_to_end(&mut rest)
        .map_err(|err| io_error(name, &err))?;
    head.extend(rest);
    let summary = inspect_glb(&head, bytes).map_err(bad)?;
    Ok(KitModel {
        file: name.to_string(),
        path: path.to_path_buf(),
        bytes,
        summary,
    })
}

/// The cockpit: `cockpit_<faction>_<class>.glb`, else the first `cockpit_*.glb` (with a warning).
fn find_cockpit(
    folder: &KitFolder,
    manifest: &KitManifest,
    warnings: &mut Vec<KitWarning>,
) -> Option<KitModel> {
    let expected = manifest.cockpit_file();
    let name = match expected.as_deref().map(|name| (name, folder.resolve(name))) {
        Some((name, Ok(Some(_)))) => name.to_string(),
        Some((name, Err(err))) => {
            warnings.push(KitWarning::new(name, err.to_string()));
            return None;
        }
        _ => {
            let other = folder.entries().into_iter().find(|entry| {
                entry.starts_with("cockpit_") && entry.to_ascii_lowercase().ends_with(".glb")
            })?;
            warnings.push(KitWarning::new(
                &other,
                match &expected {
                    Some(expected) => format!("the cockpit should be named {expected}"),
                    None => "kit.json has no class, so the cockpit name cannot be checked".to_string(),
                },
            ));
            other
        }
    };
    let opened = folder
        .resolve(&name)
        .and_then(|path| path.ok_or_else(|| unsafe_path(&name, "vanished while opening")))
        .and_then(|path| open_model(&name, &path));
    match opened {
        Ok(model) => Some(model),
        Err(err) => {
            warnings.push(KitWarning::new(&name, format!("cockpit not loaded: {err}")));
            None
        }
    }
}

/// Reads and parses an optional sidecar; every problem is a warning.
fn read_sidecar<T>(
    folder: &KitFolder,
    name: &str,
    parse: fn(&[u8]) -> Result<T, String>,
    warnings: &mut Vec<KitWarning>,
) -> Option<T> {
    let result = folder.resolve(name).and_then(|path| match path {
        Some(path) => folder.read_capped(name, &path, MAX_SIDECAR_BYTES).map(Some),
        None => Ok(None),
    });
    match result {
        Ok(Some(bytes)) => match parse(&bytes) {
            Ok(value) => Some(value),
            Err(err) => {
                warnings.push(KitWarning::new(name, format!("ignored: {err}")));
                None
            }
        },
        Ok(None) => None,
        Err(err) => {
            warnings.push(KitWarning::new(name, format!("ignored: {err}")));
            None
        }
    }
}

fn checksum_warnings(report: &ChecksumReport, warnings: &mut Vec<KitWarning>) {
    if let Some(error) = &report.error {
        warnings.push(KitWarning::new(SUMS_FILE, format!("not checked: {error}")));
    }
    for entry in report.failures() {
        let message = match &entry.status {
            ChecksumStatus::Mismatch { actual } => {
                format!("checksum mismatch: listed {}, file is {actual}", entry.expected)
            }
            ChecksumStatus::Missing => "listed but not in the folder".to_string(),
            ChecksumStatus::Refused { reason } => format!("not checked: {reason}"),
            ChecksumStatus::Ok => continue,
        };
        warnings.push(KitWarning::new(&entry.file, message));
    }
    if !report.malformed_lines.is_empty() {
        let lines: Vec<String> = report.malformed_lines.iter().map(usize::to_string).collect();
        warnings.push(KitWarning::new(
            SUMS_FILE,
            format!("line(s) {} are not `<sha256>  <file>`", lines.join(", ")),
        ));
    }
    if report.verdict != ChecksumVerdict::Absent && report.error.is_none() {
        for name in &report.unlisted {
            warnings.push(KitWarning::new(name, "not covered by SHA256SUMS"));
        }
    }
}

fn merge_clips(model: &GlbSummary, rig: &[RigClip]) -> Vec<KitClip> {
    let by_name: BTreeMap<&str, &RigClip> = rig.iter().map(|clip| (clip.name.as_str(), clip)).collect();
    let mut clips: Vec<KitClip> = model
        .clips
        .iter()
        .map(|clip| {
            let listed = by_name.get(clip.name.as_str());
            KitClip {
                name: clip.name.clone(),
                duration: clip.duration,
                source: ClipSource::Model,
                rig_secs: listed.and_then(|r| r.secs),
                joint: listed.and_then(|r| r.joint.clone()),
            }
        })
        .collect();
    for clip in rig {
        if clips.iter().any(|c| c.name == clip.name) {
            continue;
        }
        clips.push(KitClip {
            name: clip.name.clone(),
            duration: clip.secs.unwrap_or(0.0),
            source: ClipSource::Rig,
            rig_secs: clip.secs,
            joint: clip.joint.clone(),
        });
    }
    clips
}

fn merge_sockets(
    model: &GlbSummary,
    records: &[SocketRecord],
    warnings: &mut Vec<KitWarning>,
) -> Vec<KitSocket> {
    let mut sockets: Vec<KitSocket> = model
        .sockets
        .iter()
        .map(|socket| KitSocket {
            name: socket.name.clone(),
            position: Some(socket.position()),
            parent: None,
            in_model: true,
            in_sockets_json: false,
        })
        .collect();
    for record in records {
        if let Some(parent) = &record.parent
            && !model.nodes.contains(parent)
        {
            warnings.push(KitWarning::new(
                SOCKETS_FILE,
                format!(
                    "socket `{}`: parent `{parent}` is not a node of the model",
                    record.name
                ),
            ));
        }
        if let Some(socket) = sockets.iter_mut().find(|s| s.name == record.name) {
            socket.in_sockets_json = true;
            socket.parent.clone_from(&record.parent);
        } else {
            sockets.push(KitSocket {
                name: record.name.clone(),
                position: record.position,
                parent: record.parent.clone(),
                in_model: false,
                in_sockets_json: true,
            });
        }
    }
    sockets
}

impl Kit {
    /// The kit as the agent protocol and the panel report it.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let model = |m: &KitModel| {
            json!({
                "file": m.file,
                "bytes": m.bytes,
                "nodes": m.summary.nodes.len(),
                "mesh_nodes": m.summary.mesh_nodes,
                "skins": m.summary.skins,
                "skeleton_root": m.summary.skeleton_root,
                "lod_levels": m.summary.lod_levels,
            })
        };
        json!({
            "path": self.root.display().to_string(),
            "manifest": self.manifest,
            "model": model(&self.model),
            "cockpit": self.cockpit.as_ref().map(|c| {
                let mut value = model(c);
                value["attached_to"] = if self.cockpit_mount.is_some() {
                    json!(COCKPIT_SOCKET)
                } else {
                    json!("origin")
                };
                value
            }),
            "character_kit": self.character_kit,
            "clips": self.clips,
            "sockets": self.sockets,
            "checksums": self.checksums,
            "warnings": self.warnings,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// SHA-256 (FIPS 180-4)
// ---------------------------------------------------------------------------------------------

const K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

const H0: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// Streaming SHA-256 (FIPS 180-4 § 6.2).
#[derive(Debug, Clone)]
pub struct Sha256 {
    state: [u32; 8],
    block: [u8; 64],
    filled: usize,
    length: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    /// A fresh hasher.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: H0,
            block: [0; 64],
            filled: 0,
            length: 0,
        }
    }

    /// Feeds bytes.
    pub fn update(&mut self, mut data: &[u8]) {
        self.length = self.length.wrapping_add(data.len() as u64);
        while !data.is_empty() {
            let take = (64 - self.filled).min(data.len());
            self.block[self.filled..self.filled + take].copy_from_slice(&data[..take]);
            self.filled += take;
            data = &data[take..];
            if self.filled == 64 {
                let block = self.block;
                self.compress(&block);
                self.filled = 0;
            }
        }
    }

    /// The digest.
    #[must_use]
    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.length.wrapping_mul(8);
        self.update(&[0x80]);
        while self.filled != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        let mut out = [0u8; 32];
        for (chunk, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            *chunk = word.to_be_bytes();
        }
        out
    }

    /// The digest of `data` in one call.
    #[must_use]
    pub fn digest(data: &[u8]) -> [u8; 32] {
        let mut hasher = Self::new();
        hasher.update(data);
        hasher.finish()
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (t, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[t] = u32::from_be_bytes(*word);
        }
        for t in 16..64 {
            let s0 = w[t - 15].rotate_right(7) ^ w[t - 15].rotate_right(18) ^ (w[t - 15] >> 3);
            let s1 = w[t - 2].rotate_right(17) ^ w[t - 2].rotate_right(19) ^ (w[t - 2] >> 10);
            w[t] = w[t - 16].wrapping_add(s0).wrapping_add(w[t - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for t in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[t])
                .wrapping_add(w[t]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
}

/// Lowercase hex of `bytes`.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_fips_180_examples() {
        assert_eq!(
            hex(&Sha256::digest(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&Sha256::digest(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // One million `a`, fed in uneven pieces (exercises the block buffering).
        let mut hasher = Sha256::new();
        let piece = [b'a'; 997];
        let mut left = 1_000_000usize;
        while left > 0 {
            let take = left.min(piece.len());
            hasher.update(&piece[..take]);
            left -= take;
        }
        assert_eq!(
            hex(&hasher.finish()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn sums_parse_gnu_and_bsd_lines_and_flag_the_rest() {
        let digest = "a".repeat(64);
        let text = format!(
            "# kit\n{digest}  kit.json\n{d2} *./x.glb\nSHA256 (rig.json) = {d3}\nnot a line\n{short}  y\n\n",
            d2 = "B".repeat(64),
            d3 = "c".repeat(64),
            short = "a".repeat(10),
        );
        let (entries, bad) = parse_sha256sums(&text);
        let files: Vec<&str> = entries.iter().map(|e| e.file.as_str()).collect();
        assert_eq!(files, ["kit.json", "x.glb", "rig.json"]);
        assert_eq!(entries[1].expected, "b".repeat(64), "digests are lowercased");
        assert_eq!(bad, vec![5, 6]);
    }

    #[test]
    fn flat_names_only() {
        for good in ["kit.json", "a b.glb", "cockpit_jmc_heavy.glb"] {
            assert!(check_flat_name(good).is_ok(), "{good}");
        }
        for bad in ["", "..", ".", "../x", "a/b", "a\\b", "/etc/passwd", "C:x", "x\0y"] {
            assert!(check_flat_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn listed_names_allow_in_folder_subpaths_only() {
        for good in ["kit.json", "textures/x.png", "a/b/c d.bin"] {
            assert!(check_listed_name(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            ".",
            "..",
            "../x",
            "textures/../x",
            "textures/./x",
            "/etc/passwd",
            "textures//x",
            "textures/",
            "textures\\x.png",
            "C:x",
            "C:/x",
            "x:y/z",
            "x\0y",
        ] {
            assert!(check_listed_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn generic_is_a_known_faction() {
        let (manifest, warnings) = KitManifest::parse(
            br#"{"kit_id":"gun_a","kind":"weapon","faction":"generic","stats_from":"gun","version":1}"#,
        )
        .expect("manifest");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(manifest.cockpit_file(), None);
        let (manifest, _) = KitManifest::parse(
            br#"{"kit_id":"x","kind":"hull","class":"heavy","faction":"generic","stats_from":"s","version":1}"#,
        )
        .expect("manifest");
        assert_eq!(
            manifest.cockpit_file().as_deref(),
            Some("cockpit_generic_heavy.glb")
        );
    }

    #[test]
    fn manifest_requires_fields_and_warns_on_unknown_values() {
        let (manifest, warnings) = KitManifest::parse(
            br#"{"kit_id":"jmc_hauler_v3","kind":"hull","class":"heavy","faction":"jmc",
                "replaces":null,"stats_from":"jmc_hauler","contract":"production-plan 2026-10-18","version":3}"#,
        )
        .expect("manifest");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(manifest.replaces, "");
        assert_eq!(manifest.cockpit_file().as_deref(), Some("cockpit_jmc_heavy.glb"));

        let (_, warnings) =
            KitManifest::parse(br#"{"kit_id":"x","kind":"boat","faction":"zzz","version":1,"color":"red"}"#)
                .expect("manifest with odd values");
        let text: Vec<String> = warnings.iter().map(ToString::to_string).collect();
        assert!(text.iter().any(|w| w.contains("unknown key `color`")), "{text:?}");
        assert!(text.iter().any(|w| w.contains("kind `boat`")), "{text:?}");
        assert!(text.iter().any(|w| w.contains("faction `zzz`")), "{text:?}");
        assert!(text.iter().any(|w| w.contains("stats_from is empty")), "{text:?}");

        assert_eq!(
            KitManifest::parse(br#"{"kind":"hull","faction":"jmc","version":1}"#)
                .map(|_| ())
                .expect_err("must fail")
                .code(),
            "bad_manifest"
        );
        assert_eq!(
            KitManifest::parse(br#"{"kit_id":"../../evil","kind":"hull","faction":"jmc","version":1}"#)
                .map(|_| ())
                .expect_err("must fail")
                .code(),
            "invalid_kit_id"
        );
        assert!(KitManifest::parse(b"[1,2]").is_err());
        assert!(KitManifest::parse(b"{").is_err());
    }

    #[test]
    fn rig_and_socket_sidecars_parse_every_shape() {
        let rig = parse_rig(
            br#"{"stems":{"hauler":{"clips":[{"name":"canopy_open","joint":"canopy","secs":1.5},
                {"name":"gear_deploy"}]},"note":"x"}}"#,
        )
        .expect("rig");
        assert_eq!(rig.len(), 2);
        assert_eq!(rig[0].secs, Some(1.5));
        assert_eq!(rig[0].joint.as_deref(), Some("canopy"));
        assert_eq!(rig[1].secs, None);
        let flat = parse_rig(br#"{"hauler":{"clips":[{"name":"door_cycle","secs":2}]}}"#).expect("flat rig");
        assert_eq!(flat[0].name, "door_cycle");
        assert!(parse_rig(br#"{"stems":{"a":{"clips":[{"secs":1}]}}}"#).is_err());

        let list = parse_sockets(br#"{"sockets":[{"name":"socket_a","parent":"hull","position":[1,2,3]}]}"#)
            .expect("list");
        assert_eq!(list[0].position, Some([1.0, 2.0, 3.0]));
        let map =
            parse_sockets(br#"{"sockets":{"socket_b":{"node":"hull","translation":[0,1,0]}}}"#).expect("map");
        assert_eq!(map[0].name, "socket_b");
        assert_eq!(map[0].parent.as_deref(), Some("hull"));
        let flat = parse_sockets(br#"{"format":1,"socket_c":{"matrix":[1,0,0,0,0,1,0,0,0,0,1,0,4,5,6,1]}}"#)
            .expect("flat");
        assert_eq!(flat[0].position, Some([4.0, 5.0, 6.0]));
        assert!(parse_sockets(br#"{"sockets":7}"#).is_err());
    }

    #[test]
    fn lod_suffixes() {
        assert_eq!(lod_suffix("SM_hauler_hull_LOD0"), Some(0));
        assert_eq!(lod_suffix("hull_lod12"), Some(12));
        assert_eq!(lod_suffix("hull"), None);
        assert_eq!(lod_suffix("LOD1"), None);
        assert_eq!(lod_suffix("hull_LOD"), None);
        assert_eq!(lod_suffix(""), None);
    }

    fn glb(json: &Value, bin: &[u8]) -> Vec<u8> {
        let mut text = serde_json::to_vec(json).expect("json");
        while !text.len().is_multiple_of(4) {
            text.push(b' ');
        }
        let mut out = Vec::new();
        let total = 12 + 8 + text.len() + if bin.is_empty() { 0 } else { 8 + bin.len() };
        out.extend_from_slice(b"glTF");
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(&CHUNK_JSON.to_le_bytes());
        out.extend_from_slice(&text);
        if !bin.is_empty() {
            out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
            out.extend_from_slice(&CHUNK_BIN.to_le_bytes());
            out.extend_from_slice(bin);
        }
        out
    }

    #[test]
    fn glb_inspection_reads_sockets_skins_clips_and_refuses_bad_claims() {
        let document = json!({
            "asset": {"version": "2.0"},
            "nodes": [
                {"name": "hull_LOD0", "mesh": 0, "children": [1, 2]},
                {"name": "socket_cockpit", "translation": [0.0, 2.0, 1.0]},
                {"name": "root", "extras": {"tincal_kit": 1}, "translation": [1.0, 0.0, 0.0], "children": [3]},
                {"name": "child", "children": [2]},
                {"name": "hull_LOD1", "mesh": 0}
            ],
            "skins": [{"joints": [2, 3]}],
            "accessors": [{"max": [1.25]}, {"max": [0.5]}],
            "animations": [
                {"name": "idle", "samplers": [{"input": 0}, {"input": 1}], "channels": [{}, {}]},
                {"samplers": [{"input": 9}], "channels": []}
            ]
        });
        let bytes = glb(&document, &[0; 8]);
        let summary = inspect_glb(&bytes, bytes.len() as u64).expect("summary");
        assert_eq!(summary.nodes.len(), 5);
        assert_eq!(summary.mesh_nodes, 2);
        assert_eq!(summary.lod_levels, vec![0, 1]);
        assert_eq!(
            summary.socket(COCKPIT_SOCKET).map(GlbSocket::position),
            Some([0.0, 2.0, 1.0])
        );
        assert_eq!(summary.skeleton_root.as_deref(), Some("root"));
        assert!(summary.tincal_kit_marker);
        assert_eq!(summary.clips[0].name, "idle");
        assert!((summary.clips[0].duration - 1.25).abs() < 1e-6);
        assert_eq!(summary.clips[1].name, "animation_1");
        assert_eq!(summary.clips[1].duration, 0.0);

        // The header claims more than the file has.
        assert!(inspect_glb(&bytes, 30).expect_err("must fail").contains("claims"));
        let mut huge = bytes.clone();
        huge[8..12].copy_from_slice(&(2_000_000_000u32).to_le_bytes());
        assert!(
            inspect_glb(&huge, bytes.len() as u64)
                .expect_err("must fail")
                .contains("claims")
        );
        let mut huge_json = bytes.clone();
        huge_json[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(glb_head_len(&huge_json).is_err());
        let mut huge_bin = bytes.clone();
        let bin_at = bytes.len() - 16;
        huge_bin[bin_at..bin_at + 4].copy_from_slice(&(1u32 << 31).to_le_bytes());
        assert!(
            inspect_glb(&huge_bin, bytes.len() as u64)
                .expect_err("must fail")
                .contains("BIN")
        );
        assert!(inspect_glb(b"not a glb at all, really", 24).is_err());

        let outside = glb(
            &json!({"asset": {"version": "2.0"}, "buffers": [{"uri": "../secret.bin", "byteLength": 4}]}),
            &[],
        );
        assert!(
            inspect_glb(&outside, outside.len() as u64)
                .expect_err("must fail")
                .contains("outside")
        );
        let embedded = glb(
            &json!({"asset": {"version": "2.0"}, "images": [{"uri": "data:image/png;base64,AAAA"}]}),
            &[],
        );
        assert!(inspect_glb(&embedded, embedded.len() as u64).is_ok());
    }
}
