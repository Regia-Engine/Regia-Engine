// SPDX-License-Identifier: MPL-2.0
//! The asset id manifest `.assetids.ron` v1 (`docs/formats/assetids-ron.md`, ADR-8460): the
//! committed record of the ids of sources that are not in version control.
//!
//! A source's [`AssetId`] lives in its `.meta` sidecar. When the binaries (textures, glTF files)
//! are kept out of version control, their sidecars often are too, and a cook on a fresh checkout
//! mints new ids: every pak then changes and a patch ships them whole. The manifest is one row per
//! source (path under `assets/`, id, importer); [`export_asset_ids`] writes it from the sidecars on
//! disk and [`import_asset_ids`] writes back the sidecars that are missing, so the next cook keeps
//! the ids. An import never overwrites a sidecar that names another id (the conflict is reported,
//! [`CODE_ASSET_ID_CONFLICT`]) and writes nothing when any row conflicts.
//!
//! Behavior derived from the requirements of the stable-ids step of a shipping game's cook script
//! (provenance row `asset-ids-manifest`); the code is original.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::id::AssetId;
use crate::meta::{MetaSidecar, meta_path_for};

/// Current `.assetids.ron` format version (v1, ADR-8460).
pub const ASSET_IDS_FORMAT_VERSION: u32 = 1;

/// The file suffix of an asset id manifest.
pub const ASSET_IDS_SUFFIX: &str = ".assetids.ron";

/// The manifest `regia assets ids export` writes when no `--out` is given, relative to the project.
pub const DEFAULT_ASSET_IDS_FILE: &str = "asset_ids.assetids.ron";

/// Error code: a row's source already has a sidecar with another id, or one that does not read.
pub const CODE_ASSET_ID_CONFLICT: &str = "asset_id_conflict";
/// Error code: the manifest does not parse, names a newer version, or breaks a rule (bad path,
/// duplicate path, nil id, empty importer).
pub const CODE_ASSET_IDS_INVALID: &str = "asset_ids_invalid";
/// Error code: the manifest file does not read.
pub const CODE_ASSET_IDS_UNREADABLE: &str = "asset_ids_unreadable";
/// Error code: an export found a `.meta` sidecar that does not parse (its id would be lost).
pub const CODE_ASSET_IDS_BAD_META: &str = "asset_ids_bad_meta";
/// Error code: a sidecar or the manifest could not be written.
pub const CODE_ASSET_IDS_WRITE_FAILED: &str = "asset_ids_write_failed";

/// Every error code of this module, for the book and the agent contract.
pub const ASSET_IDS_CODES: &[&str] = &[
    CODE_ASSET_ID_CONFLICT,
    CODE_ASSET_IDS_INVALID,
    CODE_ASSET_IDS_UNREADABLE,
    CODE_ASSET_IDS_BAD_META,
    CODE_ASSET_IDS_WRITE_FAILED,
];

/// One source's recorded id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetIdEntry {
    /// The source path relative to the project's `assets/` folder, `/`-separated
    /// (`textures/rock.png`).
    pub path: String,
    /// The source's primary id (its `.meta` `id`). Sub-asset ids derive from it.
    pub id: AssetId,
    /// The importer its `.meta` names (`png`, `gltf`, `material_ron`, ...).
    pub importer: String,
}

/// The `.assetids.ron` document: the recorded ids, sorted by path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetIdsManifest {
    /// Format version, [`ASSET_IDS_FORMAT_VERSION`].
    pub format_version: u32,
    /// One row per source. Written sorted by `path`; a reader accepts any order.
    #[serde(default)]
    pub entries: Vec<AssetIdEntry>,
}

impl Default for AssetIdsManifest {
    fn default() -> Self {
        Self {
            format_version: ASSET_IDS_FORMAT_VERSION,
            entries: Vec::new(),
        }
    }
}

/// Errors reading, exporting or importing an asset id manifest.
#[derive(Debug, Error)]
pub enum AssetIdsError {
    /// The manifest does not parse or breaks a rule.
    #[error("asset id manifest is invalid: {0}")]
    Invalid(String),
    /// The manifest names a version this build does not read.
    #[error("asset id manifest version {found} is newer than this build reads ({supported})")]
    UnsupportedVersion {
        /// The version in the file.
        found: u32,
        /// [`ASSET_IDS_FORMAT_VERSION`].
        supported: u32,
    },
    /// The manifest file does not read.
    #[error("cannot read {path}: {source}")]
    Unreadable {
        /// The manifest path.
        path: PathBuf,
        /// The IO error.
        source: std::io::Error,
    },
    /// An export met a `.meta` that does not parse.
    #[error("{path} does not read as a .meta sidecar: {reason}")]
    BadMeta {
        /// The sidecar path.
        path: PathBuf,
        /// Why it does not read.
        reason: String,
    },
    /// A file could not be written.
    #[error("cannot write {path}: {source}")]
    Write {
        /// The path being written.
        path: PathBuf,
        /// The IO error.
        source: std::io::Error,
    },
    /// An import found rows whose source already has a different (or unreadable) sidecar; nothing
    /// was written. The report lists them and what would have been written.
    #[error("{}", conflict_summary(.0))]
    Conflict(Box<ImportReport>),
}

impl AssetIdsError {
    /// The stable lowercase error code (one of [`ASSET_IDS_CODES`]).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) | Self::UnsupportedVersion { .. } => CODE_ASSET_IDS_INVALID,
            Self::Unreadable { .. } => CODE_ASSET_IDS_UNREADABLE,
            Self::BadMeta { .. } => CODE_ASSET_IDS_BAD_META,
            Self::Write { .. } => CODE_ASSET_IDS_WRITE_FAILED,
            Self::Conflict(_) => CODE_ASSET_ID_CONFLICT,
        }
    }
}

/// The one-line text of [`AssetIdsError::Conflict`]: how many rows conflict and the first one.
fn conflict_summary(report: &ImportReport) -> String {
    let first = report.conflicts.first().map_or_else(
        || "-".to_string(),
        |c| match (&c.reason, c.existing) {
            (Some(reason), _) => format!("{}: {reason}", c.path),
            (None, Some(existing)) => format!("{}: its sidecar names {existing}", c.path),
            (None, None) => c.path.clone(),
        },
    );
    format!(
        "{} row(s) conflict with the sources on disk; nothing was written (first: {first})",
        report.conflicts.len()
    )
}

/// A row whose source already has a sidecar that disagrees with the manifest, or whose id another
/// source on disk also has (one id cooks to one file, ADR-8553).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AssetIdConflict {
    /// The source path under `assets/`.
    pub path: String,
    /// The id the manifest records.
    pub recorded: AssetId,
    /// The id the sidecar on disk names; `None` when the sidecar does not read, or when the
    /// conflict is a shared id (`shared_with`).
    pub existing: Option<AssetId>,
    /// Why the row conflicts when it is not a plain id mismatch: the sidecar does not read, or
    /// another source on disk has the same id (the text names that source's path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The other source on disk (path under `assets/`) that has the recorded id, when that is
    /// the conflict (ADR-8553).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_with: Option<String>,
}

/// What an import did (or, with conflicts or a dry run, would do), each list sorted by path.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ImportReport {
    /// Sidecars written (or to be written) with the recorded id.
    pub written: Vec<String>,
    /// Sources whose sidecar already names the recorded id.
    pub kept: Vec<String>,
    /// Rows whose source is not on disk (no sidecar is written for them).
    pub missing_source: Vec<String>,
    /// Rows whose source has a sidecar with another id, or an unreadable one.
    pub conflicts: Vec<AssetIdConflict>,
    /// Whether the sidecars in `written` are on disk (false for a dry run or on a conflict).
    pub applied: bool,
}

impl AssetIdsManifest {
    /// Parses a manifest and checks its rules ([`AssetIdsManifest::validate`]).
    ///
    /// # Errors
    ///
    /// [`AssetIdsError::UnsupportedVersion`] for a newer file, [`AssetIdsError::Invalid`] for text
    /// that does not parse or breaks a rule.
    pub fn from_ron(text: &str) -> Result<Self, AssetIdsError> {
        let version = regia_core::version::peek_ron_version(text).map_err(AssetIdsError::Invalid)?;
        if version > ASSET_IDS_FORMAT_VERSION {
            return Err(AssetIdsError::UnsupportedVersion {
                found: version,
                supported: ASSET_IDS_FORMAT_VERSION,
            });
        }
        let manifest: Self = ron::from_str(text).map_err(|e| AssetIdsError::Invalid(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Reads and parses a manifest file.
    ///
    /// # Errors
    ///
    /// [`AssetIdsError::Unreadable`] when the file does not read, else as [`Self::from_ron`].
    pub fn read(path: &Path) -> Result<Self, AssetIdsError> {
        let bytes = std::fs::read(path).map_err(|source| AssetIdsError::Unreadable {
            path: path.to_path_buf(),
            source,
        })?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|e| AssetIdsError::Invalid(format!("{}: not UTF-8: {e}", path.display())))?;
        Self::from_ron(text)
    }

    /// Checks the rules: version 1, every path a safe `/`-separated relative path that is not a
    /// sidecar, no path twice (also not twice under case folding, so a case-insensitive checkout
    /// cannot hold one file for two rows, ADR-8553), no nil id, no empty importer. Two paths may
    /// share an id (a JPEG that replaced a PNG keeps the PNG's id); [`import_asset_ids`] refuses
    /// it when both sources are on disk.
    ///
    /// # Errors
    ///
    /// [`AssetIdsError::Invalid`] naming the first broken rule.
    pub fn validate(&self) -> Result<(), AssetIdsError> {
        if self.format_version < ASSET_IDS_FORMAT_VERSION {
            return Err(AssetIdsError::Invalid(format!(
                "format_version {} is not a version (the first is {ASSET_IDS_FORMAT_VERSION})",
                self.format_version
            )));
        }
        if self.format_version > ASSET_IDS_FORMAT_VERSION {
            return Err(AssetIdsError::UnsupportedVersion {
                found: self.format_version,
                supported: ASSET_IDS_FORMAT_VERSION,
            });
        }
        let mut seen = BTreeSet::new();
        let mut folded: BTreeMap<String, &str> = BTreeMap::new();
        for entry in &self.entries {
            if let Err(why) = check_rel_path(&entry.path) {
                return Err(AssetIdsError::Invalid(format!("path {:?}: {why}", entry.path)));
            }
            if !seen.insert(entry.path.as_str()) {
                return Err(AssetIdsError::Invalid(format!(
                    "path {:?} is listed twice",
                    entry.path
                )));
            }
            if let Some(other) = folded.insert(fold_case(&entry.path), entry.path.as_str()) {
                return Err(AssetIdsError::Invalid(format!(
                    "paths {other:?} and {:?} differ only in letter case: a case-insensitive checkout \
                     (Windows, macOS) holds one file for both; rename one",
                    entry.path
                )));
            }
            if entry.id.is_nil() {
                return Err(AssetIdsError::Invalid(format!(
                    "path {:?} has the nil id",
                    entry.path
                )));
            }
            if entry.importer.trim().is_empty() {
                return Err(AssetIdsError::Invalid(format!(
                    "path {:?} has no importer",
                    entry.path
                )));
            }
        }
        Ok(())
    }

    /// The canonical text: one row per line, sorted by path, so the file diffs one line per
    /// changed source and two exports of the same sidecars are byte-equal.
    #[must_use]
    pub fn to_ron(&self) -> String {
        let mut entries: Vec<&AssetIdEntry> = self.entries.iter().collect();
        entries.sort();
        let mut out = String::new();
        out.push_str("(\n");
        out.push_str(&format!("    format_version: {},\n", self.format_version));
        out.push_str("    entries: [\n");
        for entry in entries {
            out.push_str(&format!(
                "        (path: {}, id: \"{}\", importer: {}),\n",
                ron_string(&entry.path),
                entry.id,
                ron_string(&entry.importer)
            ));
        }
        out.push_str("    ],\n)\n");
        out
    }
}

/// A RON string literal for `text` (quoted and escaped).
fn ron_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `path` with every character case-folded, for the duplicate check of
/// [`AssetIdsManifest::validate`] (paths stay case-preserving and case-sensitive on disk).
///
/// ASCII letters fold to lowercase. Any other character folds to the lowercase of its uppercase
/// when each step maps to one character (Rust's Unicode tables), else to itself; three pairs that
/// Unicode simple case folding equates and that round trip misses are mapped by hand. This
/// equates every pair Unicode simple case folding (`CaseFolding.txt` status C and S) equates, and
/// also dotless `ı` with `i` (an uppercase compare equates those), which errs toward refusing.
/// Unicode normalization (NFC/NFD) is not applied.
fn fold_case(path: &str) -> String {
    fn single(mut chars: impl Iterator<Item = char>) -> Option<char> {
        let first = chars.next()?;
        chars.next().is_none().then_some(first)
    }
    path.chars()
        .map(|c| {
            if c.is_ascii() {
                return c.to_ascii_lowercase();
            }
            match c {
                // Simple folding status S entries whose uppercase is not one character.
                '\u{1FD3}' => return '\u{0390}',
                '\u{1FE3}' => return '\u{03B0}',
                '\u{FB05}' => return '\u{FB06}',
                _ => {}
            }
            let upper = single(c.to_uppercase()).unwrap_or(c);
            single(upper.to_lowercase()).unwrap_or(upper)
        })
        .collect()
}

/// Why `path` is not a safe manifest path, if it is not.
fn check_rel_path(path: &str) -> Result<(), &'static str> {
    if path.is_empty() {
        return Err("is empty");
    }
    if path.contains('\\') {
        return Err("uses `\\` (paths are `/`-separated)");
    }
    if path.starts_with('/') || path.contains(':') {
        return Err("is absolute");
    }
    if path.chars().any(char::is_control) {
        return Err("holds a control character");
    }
    if path.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return Err("has an empty, `.` or `..` component");
    }
    if is_meta_name(path) {
        return Err("names a .meta sidecar, not a source");
    }
    Ok(())
}

/// Whether `name` ends with `.meta` in any case (the cook's discovery skips those names too).
fn is_meta_name(name: &str) -> bool {
    name.len() >= 5
        && name.is_char_boundary(name.len() - 5)
        && name[name.len() - 5..].eq_ignore_ascii_case(".meta")
}

/// Whether a directory name is a staging directory of `regia upgrade-project --write`
/// (`.<stem>.<pid>.stage`, ADR-8861): scans skip them, as the cook does.
fn is_upgrade_stage_dir_name(name: &str) -> bool {
    let Some(inner) = name.strip_prefix('.').and_then(|n| n.strip_suffix(".stage")) else {
        return false;
    };
    inner.rsplit_once('.').is_some_and(|(stem, pid)| {
        !stem.is_empty() && !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit())
    })
}

/// Every `.meta` sidecar under `root` whose source exists, as (source path relative to `root`,
/// sidecar path), sorted. Symbolic links are not followed; AppleDouble `._*` files and upgrade
/// staging directories are skipped.
fn sidecars_under(root: &Path) -> Result<Vec<(String, PathBuf)>, AssetIdsError> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let read = match std::fs::read_dir(&dir) {
            Ok(read) => read,
            Err(e) if dir == root && e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(source) => return Err(AssetIdsError::Unreadable { path: dir, source }),
        };
        for entry in read {
            // A folder that does not list fully could hide sidecars: never drop an id silently.
            let entry = entry.map_err(|source| AssetIdsError::Unreadable {
                path: dir.clone(),
                source,
            })?;
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let Ok(name) = entry.file_name().into_string() else {
                return Err(AssetIdsError::BadMeta {
                    path: entry.path(),
                    reason: "the path is not UTF-8, so the manifest cannot record it".to_string(),
                });
            };
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if kind.is_dir() {
                if !is_upgrade_stage_dir_name(&name) {
                    stack.push((entry.path(), rel));
                }
                continue;
            }
            if !kind.is_file() || name.starts_with("._") || !is_meta_name(&name) {
                continue;
            }
            let source_rel = rel[..rel.len() - 5].to_string();
            if source_rel.ends_with('/') || source_rel.is_empty() {
                continue;
            }
            let source = dir.join(&name[..name.len() - 5]);
            if source.exists() {
                out.push((source_rel, entry.path()));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The manifest of every sidecar under `assets_root` (the project's `assets/` folder) whose source
/// is on disk: path, id and importer, sorted by path. A missing folder gives an empty manifest.
///
/// # Errors
///
/// [`AssetIdsError::BadMeta`] when a sidecar does not parse (an export never drops an id
/// silently), [`AssetIdsError::Unreadable`] when the folder does not list.
pub fn export_asset_ids(assets_root: &Path) -> Result<AssetIdsManifest, AssetIdsError> {
    let _span = tracing::info_span!(target: "regia_asset", "asset_ids_export").entered();
    let mut entries = Vec::new();
    for (path, meta_file) in sidecars_under(assets_root)? {
        let bytes = std::fs::read(&meta_file).map_err(|e| AssetIdsError::BadMeta {
            path: meta_file.clone(),
            reason: e.to_string(),
        })?;
        let meta = MetaSidecar::from_ron(&bytes).map_err(|e| AssetIdsError::BadMeta {
            path: meta_file.clone(),
            reason: e.to_string(),
        })?;
        entries.push(AssetIdEntry {
            path,
            id: meta.id,
            importer: meta.importer,
        });
    }
    entries.sort();
    let manifest = AssetIdsManifest {
        format_version: ASSET_IDS_FORMAT_VERSION,
        entries,
    };
    manifest.validate()?;
    Ok(manifest)
}

/// Writes the manifest's missing sidecars under `assets_root` (the project's `assets/` folder).
///
/// For each row: a source that is not on disk is skipped (`missing_source`); a sidecar that
/// already names the recorded id is kept; a sidecar with another id, or one that does not read, is
/// a conflict; otherwise a `.meta` with the recorded id and importer is written (the text the
/// cook writes for a new source). With any conflict nothing is written and the call fails with
/// [`AssetIdsError::Conflict`]; with `dry_run` nothing is written and the report says what would
/// be. A sidecar is created atomically and never replaces one written meanwhile: if another
/// writer wins and its id differs, that row is a conflict too.
///
/// # Errors
///
/// [`AssetIdsError::Conflict`] (code [`CODE_ASSET_ID_CONFLICT`]) or
/// [`AssetIdsError::Write`].
pub fn import_asset_ids(
    assets_root: &Path,
    manifest: &AssetIdsManifest,
    dry_run: bool,
) -> Result<ImportReport, AssetIdsError> {
    let _span = tracing::info_span!(target: "regia_asset", "asset_ids_import").entered();
    manifest.validate()?;
    let mut report = ImportReport::default();
    let mut to_write: Vec<(&AssetIdEntry, PathBuf)> = Vec::new();
    let mut entries: Vec<&AssetIdEntry> = manifest.entries.iter().collect();
    entries.sort();
    for entry in entries {
        let source = entry
            .path
            .split('/')
            .fold(assets_root.to_path_buf(), |path, part| path.join(part));
        if !source.exists() {
            report.missing_source.push(entry.path.clone());
            continue;
        }
        let meta_file = meta_path_for(&source);
        match existing_id(&meta_file) {
            Existing::None => to_write.push((entry, meta_file)),
            Existing::Id(id) if id == entry.id => report.kept.push(entry.path.clone()),
            Existing::Id(id) => report.conflicts.push(AssetIdConflict {
                path: entry.path.clone(),
                recorded: entry.id,
                existing: Some(id),
                reason: None,
                shared_with: None,
            }),
            Existing::Unreadable(reason) => report.conflicts.push(AssetIdConflict {
                path: entry.path.clone(),
                recorded: entry.id,
                existing: None,
                reason: Some(reason),
                shared_with: None,
            }),
        }
    }
    report.written = to_write.iter().map(|(e, _)| e.path.clone()).collect();
    report.conflicts.extend(shared_id_conflicts(
        assets_root,
        manifest,
        &report.missing_source,
        &to_write,
    )?);
    report.conflicts.sort_by(|a, b| a.path.cmp(&b.path));
    if !report.conflicts.is_empty() {
        return Err(AssetIdsError::Conflict(Box::new(report)));
    }
    if dry_run {
        return Ok(report);
    }
    let mut lost_races = Vec::new();
    for (entry, meta_file) in &to_write {
        let text = MetaSidecar::new(&entry.importer, entry.id)
            .to_ron_string()
            .map_err(|e| AssetIdsError::Invalid(format!("path {:?}: {e}", entry.path)))?;
        let created =
            write_new_atomic(meta_file, text.as_bytes()).map_err(|source| AssetIdsError::Write {
                path: meta_file.clone(),
                source,
            })?;
        if !created {
            lost_races.push((*entry, meta_file.clone()));
        }
    }
    // A sidecar another writer created between the check and the write: kept when it agrees.
    for (entry, meta_file) in lost_races {
        report.written.retain(|p| p != &entry.path);
        match existing_id(&meta_file) {
            Existing::Id(id) if id == entry.id => report.kept.push(entry.path.clone()),
            Existing::Id(id) => report.conflicts.push(AssetIdConflict {
                path: entry.path.clone(),
                recorded: entry.id,
                existing: Some(id),
                reason: None,
                shared_with: None,
            }),
            Existing::Unreadable(reason) => report.conflicts.push(AssetIdConflict {
                path: entry.path.clone(),
                recorded: entry.id,
                existing: None,
                reason: Some(reason),
                shared_with: None,
            }),
            Existing::None => report.conflicts.push(AssetIdConflict {
                path: entry.path.clone(),
                recorded: entry.id,
                existing: None,
                reason: Some("the sidecar appeared and vanished during the import".to_string()),
                shared_with: None,
            }),
        }
    }
    report.kept.sort();
    report.applied = true;
    if !report.conflicts.is_empty() {
        return Err(AssetIdsError::Conflict(Box::new(report)));
    }
    Ok(report)
}

/// Rows whose id another source on disk also has: the cook names its output `{id}.fckd`, so two
/// present sources with one id would cook to one file (ADR-8553). Two kinds:
///
/// - two manifest rows with one id whose sources are both on disk (a shared id is fine while
///   the replaced source is gone, so rows in `missing` do not count);
/// - a row about to get a new sidecar whose id a sidecar outside the manifest already names.
///
/// Every row involved is listed, each naming the other path.
fn shared_id_conflicts(
    assets_root: &Path,
    manifest: &AssetIdsManifest,
    missing: &[String],
    to_write: &[(&AssetIdEntry, PathBuf)],
) -> Result<Vec<AssetIdConflict>, AssetIdsError> {
    let shared = |path: &str, id: AssetId, other: &str| AssetIdConflict {
        path: path.to_string(),
        recorded: id,
        existing: None,
        reason: Some(format!(
            "{other} is also on disk with id {id}: one id cooks to one file; remove the replaced \
             source or give one of them a new id"
        )),
        shared_with: Some(other.to_string()),
    };
    let mut out = Vec::new();
    let mut by_id: BTreeMap<AssetId, Vec<&str>> = BTreeMap::new();
    for entry in &manifest.entries {
        if !missing.contains(&entry.path) {
            by_id.entry(entry.id).or_default().push(entry.path.as_str());
        }
    }
    for (id, mut paths) in by_id {
        if paths.len() < 2 {
            continue;
        }
        paths.sort_unstable();
        for (i, &path) in paths.iter().enumerate() {
            let other = paths[usize::from(i == 0)];
            out.push(shared(path, id, other));
        }
    }
    if to_write.is_empty() {
        return Ok(out);
    }
    let rows: BTreeSet<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
    let new_ids: BTreeMap<AssetId, &str> = to_write.iter().map(|(e, _)| (e.id, e.path.as_str())).collect();
    for (source, meta_file) in sidecars_under(assets_root)? {
        if rows.contains(source.as_str()) {
            continue;
        }
        if let Existing::Id(id) = existing_id(&meta_file)
            && let Some(&row) = new_ids.get(&id)
        {
            out.push(shared(row, id, &source));
        }
    }
    Ok(out)
}

/// What a sidecar path holds.
enum Existing {
    None,
    Id(AssetId),
    Unreadable(String),
}

fn existing_id(meta_file: &Path) -> Existing {
    match std::fs::read(meta_file) {
        Ok(bytes) => match MetaSidecar::from_ron(&bytes) {
            Ok(meta) => Existing::Id(meta.id),
            Err(e) => Existing::Unreadable(e.to_string()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Existing::None,
        Err(e) => Existing::Unreadable(e.to_string()),
    }
}

static NEXT_TMP: AtomicU64 = AtomicU64::new(0);

/// A temporary name next to `path`, unique per process and call.
fn tmp_beside(path: &Path) -> PathBuf {
    let n = NEXT_TMP.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}-{n}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// Creates `path` with `bytes` atomically, never replacing a file that exists: `Ok(false)` when
/// one does (or appeared meanwhile). Falls back to an exclusive create where hard links are not
/// supported.
fn write_new_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<bool> {
    let tmp = tmp_beside(path);
    if let Err(error) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(_) => {
            use std::io::Write as _;
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(mut file) => {
                    if let Err(e) = file.write_all(bytes) {
                        drop(file);
                        let _ = std::fs::remove_file(path);
                        return Err(e);
                    }
                    Ok(true)
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
                Err(e) => Err(e),
            }
        }
    }
}

/// Writes the manifest's canonical text to `path` atomically (temporary file, then rename),
/// creating the parent folder.
///
/// # Errors
///
/// [`AssetIdsError::Write`] when the file cannot be written.
pub fn write_asset_ids(path: &Path, manifest: &AssetIdsManifest) -> Result<(), AssetIdsError> {
    let err = |source| AssetIdsError::Write {
        path: path.to_path_buf(),
        source,
    };
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(err)?;
    }
    let tmp = tmp_beside(path);
    if let Err(e) = std::fs::write(&tmp, manifest.to_ron()) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err(e));
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        err(e)
    })
}

/// The serde field-list fingerprint of `.assetids.ron` (hashed by `cargo xtask check-formats`).
#[must_use]
pub fn asset_ids_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<AssetIdsManifest>(),
        regia_core::schema::serde_field_names::<AssetIdEntry>(),
    ])
}

/// The versions a migration step leaves: none yet (v1 is the first version).
#[must_use]
pub fn asset_ids_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// A small manifest every fixture (format corpus, fuzz target, docs) is built from.
#[must_use]
pub fn fixture_manifest() -> AssetIdsManifest {
    let id = |n: u128| AssetId(regia_core::id::Guid::from_u128(n));
    AssetIdsManifest {
        format_version: ASSET_IDS_FORMAT_VERSION,
        entries: vec![
            AssetIdEntry {
                path: "models/crate.glb".to_string(),
                id: id(0x5d0c_8a3e_4b1f_4c62_9e07_3a41_c2d8_7f10),
                importer: "gltf".to_string(),
            },
            AssetIdEntry {
                path: "textures/rock_albedo.jpg".to_string(),
                id: id(0x2b6e_91f4_07ad_4e35_8c19_d4a2_6b03_e85f),
                importer: "jpg".to_string(),
            },
            AssetIdEntry {
                path: "textures/rock_albedo.png".to_string(),
                id: id(0x2b6e_91f4_07ad_4e35_8c19_d4a2_6b03_e85f),
                importer: "png".to_string(),
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, bytes).expect("write");
    }

    fn meta(importer: &str, id: AssetId) -> Vec<u8> {
        MetaSidecar::new(importer, id)
            .to_ron_string()
            .expect("meta")
            .into_bytes()
    }

    /// An assets folder with three sources and their sidecars, one source without a sidecar, a
    /// sidecar without its source and an upgrade staging folder.
    fn project(label: &str) -> (PathBuf, Vec<(String, AssetId)>) {
        let root = regia_test_utils::unique_temp_dir(label);
        let assets = root.join("assets");
        let mut ids = Vec::new();
        for (path, importer) in [
            ("textures/b.png", "png"),
            ("models/a.glb", "gltf"),
            ("materials/m.material.ron", "material_ron"),
        ] {
            let id = AssetId::new_random();
            write(&assets.join(path), b"source");
            write(&meta_path_for(&assets.join(path)), &meta(importer, id));
            ids.push((path.to_string(), id));
        }
        write(&assets.join("textures/no_meta.png"), b"source");
        write(&assets.join("gone.png.meta"), &meta("png", AssetId::new_random()));
        write(
            &assets.join(".level.123.stage/x.png.meta"),
            &meta("png", AssetId::new_random()),
        );
        write(&assets.join(".level.123.stage/x.png"), b"source");
        (root, ids)
    }

    #[test]
    fn export_is_byte_stable() {
        let (root, ids) = project("asset-ids-stable");
        let assets = root.join("assets");
        let first = export_asset_ids(&assets).expect("export").to_ron();
        let second = export_asset_ids(&assets).expect("export").to_ron();
        assert_eq!(first, second, "two exports of the same sidecars differ");
        let manifest = AssetIdsManifest::from_ron(&first).expect("reads back");
        let paths: Vec<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
        // Sorted by path; only sources on disk with a sidecar; no staging folder.
        assert_eq!(
            paths,
            ["materials/m.material.ron", "models/a.glb", "textures/b.png"]
        );
        for (path, id) in &ids {
            assert!(
                manifest.entries.iter().any(|e| &e.path == path && e.id == *id),
                "{path}"
            );
        }
        assert_eq!(manifest.to_ron(), first, "the canonical text is a fixed point");
        // Written through the file helper: the same bytes.
        let out = root.join("ids").join(DEFAULT_ASSET_IDS_FILE);
        write_asset_ids(&out, &manifest).expect("write");
        assert_eq!(std::fs::read_to_string(&out).expect("read"), first);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn import_writes_only_missing_sidecars() {
        let (root, ids) = project("asset-ids-missing");
        let assets = root.join("assets");
        let manifest = export_asset_ids(&assets).expect("export");
        // Lose two sidecars; keep the third byte-for-byte to prove it is not rewritten.
        let kept_meta = meta_path_for(&assets.join(&ids[2].0));
        let kept_bytes =
            b"(format_version: 2, id: \"ID\", importer: \"material_ron\", settings: \"(quality: 9)\")"
                .to_vec();
        let kept_bytes = String::from_utf8(kept_bytes)
            .expect("utf8")
            .replace("ID", &ids[2].1.to_string())
            .into_bytes();
        std::fs::write(&kept_meta, &kept_bytes).expect("custom sidecar");
        for (path, _) in &ids[..2] {
            std::fs::remove_file(meta_path_for(&assets.join(path))).expect("rm meta");
        }
        // A row whose source is gone writes nothing.
        let mut with_missing = manifest.clone();
        with_missing.entries.push(AssetIdEntry {
            path: "textures/not_here.png".into(),
            id: AssetId::new_random(),
            importer: "png".into(),
        });
        let dry = import_asset_ids(&assets, &with_missing, true).expect("dry run");
        assert!(!dry.applied);
        assert_eq!(dry.written.len(), 2);
        assert!(
            !meta_path_for(&assets.join(&ids[0].0)).exists(),
            "a dry run wrote"
        );

        let report = import_asset_ids(&assets, &with_missing, false).expect("import");
        assert!(report.applied);
        let mut written = report.written.clone();
        written.sort();
        let mut expected: Vec<String> = ids[..2].iter().map(|(p, _)| p.clone()).collect();
        expected.sort();
        assert_eq!(written, expected);
        assert_eq!(report.kept, vec![ids[2].0.clone()]);
        assert_eq!(report.missing_source, vec!["textures/not_here.png".to_string()]);
        assert!(!assets.join("textures/not_here.png.meta").exists());
        assert!(
            !assets.join("textures/no_meta.png.meta").exists(),
            "a source the manifest does not list got a sidecar"
        );
        assert_eq!(
            std::fs::read(&kept_meta).expect("read"),
            kept_bytes,
            "an existing sidecar was rewritten"
        );
        for (path, id) in &ids {
            let bytes = std::fs::read(meta_path_for(&assets.join(path))).expect("meta");
            assert_eq!(MetaSidecar::from_ron(&bytes).expect("meta").id, *id, "{path}");
        }
        // Again: everything is kept, nothing written.
        let again = import_asset_ids(&assets, &manifest, false).expect("again");
        assert!(again.written.is_empty());
        assert_eq!(again.kept.len(), 3);
        // The export after the import equals the export before the loss.
        assert_eq!(
            export_asset_ids(&assets).expect("export").to_ron(),
            manifest.to_ron()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn import_refuses_a_conflicting_id() {
        let (root, ids) = project("asset-ids-conflict");
        let assets = root.join("assets");
        let mut manifest = export_asset_ids(&assets).expect("export");
        // One row disagrees with the sidecar on disk; another row's sidecar is missing.
        let other = AssetId::new_random();
        let conflicting = manifest
            .entries
            .iter_mut()
            .find(|e| e.path == ids[0].0)
            .expect("row");
        conflicting.id = other;
        std::fs::remove_file(meta_path_for(&assets.join(&ids[1].0))).expect("rm");
        let before = std::fs::read(meta_path_for(&assets.join(&ids[0].0))).expect("read");
        let err = import_asset_ids(&assets, &manifest, false).expect_err("conflict");
        assert_eq!(err.code(), CODE_ASSET_ID_CONFLICT);
        let AssetIdsError::Conflict(report) = &err else {
            panic!("not a conflict: {err}");
        };
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(report.conflicts[0].path, ids[0].0);
        assert_eq!(report.conflicts[0].recorded, other);
        assert_eq!(report.conflicts[0].existing, Some(ids[0].1));
        assert!(!report.applied);
        assert_eq!(
            std::fs::read(meta_path_for(&assets.join(&ids[0].0))).expect("read"),
            before,
            "the conflicting sidecar was overwritten"
        );
        assert!(
            !meta_path_for(&assets.join(&ids[1].0)).exists(),
            "an import with a conflict wrote another sidecar"
        );
        // A sidecar that does not read is a conflict too (its id is unknown, so never replaced).
        std::fs::write(meta_path_for(&assets.join(&ids[0].0)), b"not ron").expect("garbage");
        let err = import_asset_ids(&assets, &export_fixture(&ids), false).expect_err("unreadable");
        let AssetIdsError::Conflict(report) = &err else {
            panic!("not a conflict: {err}");
        };
        assert_eq!(report.conflicts[0].existing, None);
        assert!(report.conflicts[0].reason.is_some());
        assert_eq!(
            std::fs::read(meta_path_for(&assets.join(&ids[0].0))).expect("read"),
            b"not ron"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn export_fixture(ids: &[(String, AssetId)]) -> AssetIdsManifest {
        AssetIdsManifest {
            format_version: ASSET_IDS_FORMAT_VERSION,
            entries: ids
                .iter()
                .map(|(path, id)| AssetIdEntry {
                    path: path.clone(),
                    id: *id,
                    importer: "png".into(),
                })
                .collect(),
        }
    }

    #[test]
    fn export_refuses_a_sidecar_that_does_not_read() {
        let (root, ids) = project("asset-ids-bad-meta");
        let assets = root.join("assets");
        std::fs::write(meta_path_for(&assets.join(&ids[0].0)), b"(oops").expect("garbage");
        let err = export_asset_ids(&assets).expect_err("bad meta");
        assert_eq!(err.code(), CODE_ASSET_IDS_BAD_META);
        assert!(err.to_string().contains(&ids[0].0), "{err}");
        // No assets folder: an empty manifest.
        let empty = export_asset_ids(&root.join("nope")).expect("empty");
        assert!(empty.entries.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_reader_enforces_the_rules() {
        let good = fixture_manifest().to_ron();
        assert_eq!(
            AssetIdsManifest::from_ron(&good).expect("fixture"),
            fixture_manifest()
        );
        let bad = |from: &str, to: &str| {
            let text = good.replacen(from, to, 1);
            assert_ne!(text, good, "{from}");
            AssetIdsManifest::from_ron(&text).expect_err(to)
        };
        assert_eq!(
            bad("format_version: 1", "format_version: 2").code(),
            CODE_ASSET_IDS_INVALID
        );
        assert!(matches!(
            bad("format_version: 1", "format_version: 9"),
            AssetIdsError::UnsupportedVersion { found: 9, .. }
        ));
        for path in [
            "../escape.png",
            "/abs.png",
            "a//b.png",
            "./a.png",
            "a\\\\b.png",
            "C:/x.png",
            "a.png.meta",
            "",
        ] {
            bad("models/crate.glb", path);
        }
        bad("textures/rock_albedo.jpg", "textures/rock_albedo.png");
        bad("importer: \"gltf\"", "importer: \" \"");
        bad(
            "5d0c8a3e-4b1f-4c62-9e07-3a41c2d87f10",
            "00000000-0000-0000-0000-000000000000",
        );
        bad("importer: \"gltf\"", "importer: \"gltf\", extra: 1");
        // Rows in any order read; the writer sorts them.
        let mut shuffled = fixture_manifest();
        shuffled.entries.reverse();
        let text = shuffled.to_ron();
        assert_eq!(text, good);
        // Escaped strings round trip.
        let odd = AssetIdsManifest {
            format_version: 1,
            entries: vec![AssetIdEntry {
                path: "a \"quoted\" name.png".into(),
                id: AssetId::new_random(),
                importer: "png".into(),
            }],
        };
        assert_eq!(AssetIdsManifest::from_ron(&odd.to_ron()).expect("escaped"), odd);
    }

    #[test]
    fn every_code_is_lowercase() {
        for code in ASSET_IDS_CODES {
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
        }
    }

    /// Two sources on disk that share one id would cook to one `{id}.fckd`: the import refuses
    /// it, names both paths and writes nothing (ADR-8553). The replacement case (the old source
    /// gone) still imports and writes the id once.
    #[test]
    fn import_refuses_two_present_sources_with_one_id() {
        let root = regia_test_utils::unique_temp_dir("asset-ids-shared-id");
        let assets = root.join("assets");
        let manifest = fixture_manifest();
        write(&assets.join("textures/rock_albedo.jpg"), b"jpg");
        write(&assets.join("textures/rock_albedo.png"), b"png");
        let err = import_asset_ids(&assets, &manifest, false).expect_err("both sources exist");
        assert_eq!(err.code(), CODE_ASSET_ID_CONFLICT);
        let text = err.to_string();
        let AssetIdsError::Conflict(report) = &err else {
            panic!("not a conflict: {err}");
        };
        let paths: Vec<&str> = report.conflicts.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["textures/rock_albedo.jpg", "textures/rock_albedo.png"]);
        let jpg_reason = report.conflicts[0].reason.as_deref().unwrap_or_default();
        assert!(jpg_reason.contains("textures/rock_albedo.png"), "{jpg_reason}");
        assert_eq!(
            report.conflicts[0].shared_with.as_deref(),
            Some("textures/rock_albedo.png")
        );
        assert_eq!(
            report.conflicts[1].shared_with.as_deref(),
            Some("textures/rock_albedo.jpg")
        );
        assert!(
            text.contains("textures/rock_albedo.jpg") && text.contains("textures/rock_albedo.png"),
            "{text}"
        );
        assert!(!report.applied);
        for path in ["textures/rock_albedo.jpg", "textures/rock_albedo.png"] {
            assert!(
                !meta_path_for(&assets.join(path)).exists(),
                "{path}: nothing is written"
            );
        }
        // A dry run reports the same conflict.
        let dry = import_asset_ids(&assets, &manifest, true).expect_err("dry run too");
        assert_eq!(dry.code(), CODE_ASSET_ID_CONFLICT);

        // The PNG was replaced: only the JPEG is on disk. The shared id is valid and written once.
        std::fs::remove_file(assets.join("textures/rock_albedo.png")).expect("rm png");
        let report = import_asset_ids(&assets, &manifest, false).expect("jpg only imports");
        assert_eq!(report.written, ["textures/rock_albedo.jpg"]);
        assert_eq!(
            report.missing_source,
            ["models/crate.glb", "textures/rock_albedo.png"]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A row about to get a new sidecar whose id a source outside the manifest already has is the
    /// same collision.
    #[test]
    fn import_refuses_an_id_an_unlisted_source_already_has() {
        let root = regia_test_utils::unique_temp_dir("asset-ids-unlisted-id");
        let assets = root.join("assets");
        let id = AssetId::new_random();
        write(&assets.join("textures/old.png"), b"png");
        write(&meta_path_for(&assets.join("textures/old.png")), &meta("png", id));
        write(&assets.join("textures/new.jpg"), b"jpg");
        let manifest = AssetIdsManifest {
            format_version: ASSET_IDS_FORMAT_VERSION,
            entries: vec![AssetIdEntry {
                path: "textures/new.jpg".into(),
                id,
                importer: "jpg".into(),
            }],
        };
        let err = import_asset_ids(&assets, &manifest, false).expect_err("old.png has the id");
        assert_eq!(err.code(), CODE_ASSET_ID_CONFLICT);
        let text = err.to_string();
        assert!(
            text.contains("textures/new.jpg") && text.contains("textures/old.png"),
            "{text}"
        );
        let AssetIdsError::Conflict(report) = &err else {
            panic!("not a conflict: {err}");
        };
        assert_eq!(
            report.conflicts[0].shared_with.as_deref(),
            Some("textures/old.png")
        );
        assert!(!meta_path_for(&assets.join("textures/new.jpg")).exists());
        // Once the old source is gone, the id moves to the new one.
        std::fs::remove_file(assets.join("textures/old.png")).expect("rm");
        let report = import_asset_ids(&assets, &manifest, false).expect("imports");
        assert_eq!(report.written, ["textures/new.jpg"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Paths stay case-preserving and case-sensitive, but two rows equal under case folding are
    /// refused with both paths named (ADR-8553): a Windows or macOS checkout holds one file for
    /// both.
    #[test]
    fn rows_equal_under_case_folding_are_refused() {
        let row = |path: &str, id: u128| {
            format!(
                "        (path: \"{path}\", id: \"{}\", importer: \"png\"),\n",
                AssetId(regia_core::id::Guid::from_u128(id))
            )
        };
        let manifest = |rows: &[(&str, u128)]| {
            let mut text = String::from("(\n    format_version: 1,\n    entries: [\n");
            for (path, id) in rows {
                text.push_str(&row(path, *id));
            }
            text.push_str("    ],\n)\n");
            AssetIdsManifest::from_ron(&text)
        };
        let a = 0x2b6e_91f4_07ad_4e35_8c19_d4a2_6b03_e85f;
        let b = 0x1111_1111_1111_4111_8111_1111_1111_1111;
        for (first, second) in [
            ("Textures/Rock.png", "textures/rock.png"),
            ("ÉCLAIR.png", "éclair.png"),
            ("straße.png", "STRAẞE.png"),
            ("Σίσυφος.png", "σίσυφοσ.png"),
            ("\u{212A}elvin.png", "kelvin.png"),
        ] {
            let err = manifest(&[(first, a), (second, b)]).expect_err(second);
            assert_eq!(err.code(), CODE_ASSET_IDS_INVALID);
            let text = err.to_string();
            assert!(text.contains(first) && text.contains(second), "{text}");
            assert!(text.contains("letter case"), "{text}");
        }
        // Different names survive unchanged, letter case kept.
        let ok = manifest(&[("Textures/Rock.png", a), ("textures/rock2.png", b)]).expect("distinct");
        assert!(ok.to_ron().contains("Textures/Rock.png"));
        assert_eq!(fold_case("ABCxyz/É"), "abcxyz/é");
        assert_eq!(fold_case("\u{1FD3}"), fold_case("\u{0390}"));
    }
}
