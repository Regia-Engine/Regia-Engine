// SPDX-License-Identifier: MPL-2.0
//! The directory form of a level (`<name>.level.d/`): one file per entity, so version control
//! merges per entity and a save rewrites only what changed (`docs/specs/37-per-entity-levels-layers-instances.md`
//! § 3; format `level-dir` v1, `docs/formats/level-dir.md`).
//!
//! ```text
//! forest.level.d/
//!   level.ron                                   # LevelDirFile: versions + resources, no entities
//!   entities/8f/8f1c3a…(32 hex).entity.ron      # one EntityFile per entity, sharded by the first two hex digits
//! ```
//!
//! The set of entity files *is* the set of entities (no index). Reading assembles one
//! [`SceneFile`] with entities sorted by guid, so the result never depends on directory iteration
//! order, and the cooked scene of a level is the same whichever form it was authored in
//! ([`read_level_source`] is the cook input for both forms). The directory form is authoring
//! only: a shipped pak never contains it (`ship_check` rejects `*.level.d` paths).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use regia_core::id::Guid;
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::format::{SCENE_FORMAT_VERSION, SceneEntity, SceneFile, SceneFormatError};

/// Version of the `level.ron` header and entity-file wrappers in a level directory.
pub const LEVEL_DIR_FORMAT_VERSION: u32 = 1;

/// File name of a level directory's header.
pub const HEADER_FILE: &str = "level.ron";

/// The serde field-list fingerprint of the `level-dir` format (`level.ron` header and entity
/// files; hashed by `cargo xtask check-formats`).
#[must_use]
pub fn level_dir_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<LevelDirFile>(),
        regia_core::schema::serde_field_names::<EntityFile>(),
    ])
}
/// Sub-directory holding the shard directories of entity files.
pub const ENTITIES_DIR: &str = "entities";
/// Suffix of a level directory (`forest.level.d`).
pub const LEVEL_DIR_SUFFIX: &str = ".level.d";
/// Suffix of the single-file form (`forest.level.ron`).
pub const LEVEL_FILE_SUFFIX: &str = ".level.ron";
/// Suffix of one entity file.
pub const ENTITY_FILE_SUFFIX: &str = ".entity.ron";

/// Whether `name` is a staging directory of `regia upgrade-project --write`
/// (`.<name>.<pid>.stage`, ADR-8811): it holds a migrated copy of a level directory while the run
/// lasts, so project scans (the upgrade plan, the cook) never treat its contents as a second copy
/// of the level, and a later run removes one an interrupted run left (ADR-8861).
#[must_use]
pub fn is_upgrade_stage_dir_name(name: &str) -> bool {
    let Some(inner) = name.strip_prefix('.').and_then(|n| n.strip_suffix(".stage")) else {
        return false;
    };
    inner.rsplit_once('.').is_some_and(|(stem, pid)| {
        !stem.is_empty() && !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit())
    })
}

/// Directory reads switch to parallel parsing above this many entity files.
const PARALLEL_READ_THRESHOLD: usize = 256;

/// The header file of a level directory (`level.ron`). It holds everything in a [`SceneFile`]
/// except the entities.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LevelDirFile {
    /// [`LEVEL_DIR_FORMAT_VERSION`] the header was written at.
    pub format_version: u32,
    /// [`SCENE_FORMAT_VERSION`] the entity files were written at.
    pub scene_format_version: u32,
    /// Resource values by full type path, as raw RON (same as [`SceneFile::resources`]).
    #[serde(default)]
    pub resources: BTreeMap<String, Box<ron::value::RawValue>>,
}

/// One entity file of a level directory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EntityFile {
    /// [`LEVEL_DIR_FORMAT_VERSION`] the file was written at.
    pub format_version: u32,
    /// [`SCENE_FORMAT_VERSION`] of the record.
    pub scene_format_version: u32,
    /// Exactly the record a [`SceneFile`] would hold.
    pub entity: SceneEntity,
}

/// Which form a level is stored in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LevelForm {
    /// The single-file `.level.ron`.
    File,
    /// The directory `.level.d/`.
    Dir,
}

impl LevelForm {
    /// `"file"` or `"dir"` (the agent protocol's spelling).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Dir => "dir",
        }
    }
}

/// What [`read_level_dir`] read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LevelDirReport {
    /// Entities assembled.
    pub entities: usize,
    /// Files parsed (header plus entity files).
    pub files_read: usize,
    /// Non-fatal findings (junk files under `entities/`, `entity_wrong_shard`), each prefixed with
    /// its reason string.
    pub warnings: Vec<String>,
}

/// What [`write_level_dir`] changed on disk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LevelDirWriteReport {
    /// Files whose bytes changed (entity files first in guid order, the header last).
    pub written: Vec<PathBuf>,
    /// Entity files removed because their entity left the scene (or sat in the wrong shard).
    pub deleted: Vec<PathBuf>,
    /// Files whose bytes were already equal and were not touched.
    pub unchanged: usize,
}

/// The entities (and header) a save should consider: the editor's per-level dirty set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LevelDirtySet {
    /// Level resources changed: rewrite `level.ron`.
    pub header: bool,
    /// Entities changed, added or removed (removed ones are absent from the scene).
    pub entities: BTreeSet<Guid>,
}

impl LevelDirtySet {
    /// True when nothing is dirty.
    pub fn is_empty(&self) -> bool {
        !self.header && self.entities.is_empty()
    }
}

/// The editor's dirty sets by level path (either spelling is normalized to the directory path),
/// fed by reflection change tracking and scene undo. `level.storage` reports it and
/// `level.convert` refuses a dirty level (`level_dirty`). Absent at runtime.
#[derive(Resource, Clone, Debug, Default)]
pub struct LevelDirtyTracker {
    levels: BTreeMap<PathBuf, LevelDirtySet>,
}

impl LevelDirtyTracker {
    fn key(path: &Path) -> PathBuf {
        level_paths(path).1
    }

    /// Marks `level.ron` dirty (a level resource changed).
    pub fn mark_header(&mut self, level: impl AsRef<Path>) {
        self.levels.entry(Self::key(level.as_ref())).or_default().header = true;
    }

    /// Marks one entity dirty (changed, added or removed).
    pub fn mark_entity(&mut self, level: impl AsRef<Path>, guid: Guid) {
        self.levels
            .entry(Self::key(level.as_ref()))
            .or_default()
            .entities
            .insert(guid);
    }

    /// The dirty set of a level, if any.
    pub fn get(&self, level: &Path) -> Option<&LevelDirtySet> {
        self.levels.get(&Self::key(level))
    }

    /// Forgets a level's dirty set (after a save).
    pub fn clear(&mut self, level: &Path) -> Option<LevelDirtySet> {
        self.levels.remove(&Self::key(level))
    }
}

/// Errors of the level-directory reader and writer. [`LevelDirError::reason`] is the agent
/// protocol's `data.reason` string (spec 37 § 9).
#[derive(Debug, thiserror::Error)]
pub enum LevelDirError {
    /// Both `x.level.ron` and `x.level.d/` exist.
    #[error("ambiguous_level_form: both {file} and {dir} exist; delete one")]
    AmbiguousLevelForm {
        /// The single-file form.
        file: PathBuf,
        /// The directory form.
        dir: PathBuf,
    },
    /// Neither form (or no header) exists.
    #[error("level_not_found: {0}")]
    NotFound(PathBuf),
    /// An entity file's name does not match the guid it holds.
    #[error("entity_file_mismatch: {path} holds entity {guid}")]
    EntityFileMismatch {
        /// The file.
        path: PathBuf,
        /// The guid inside it.
        guid: Guid,
    },
    /// Two entity files hold the same guid.
    #[error("duplicate_entity: {guid} in {first} and {second}")]
    DuplicateEntity {
        /// The repeated guid.
        guid: Guid,
        /// First file.
        first: PathBuf,
        /// Second file.
        second: PathBuf,
    },
    /// Files of one directory declare different scene versions.
    #[error("mixed_scene_versions: {0}; run `regia upgrade-project --write`")]
    MixedSceneVersions(String),
    /// A file is newer than this build.
    #[error("unsupported_version: {path} is v{found}, this build reads up to v{supported}")]
    UnsupportedVersion {
        /// The file.
        path: PathBuf,
        /// Version found.
        found: u32,
        /// Newest supported.
        supported: u32,
    },
    /// A file is not valid RON for its type.
    #[error("level_parse_error: {path}: {message}")]
    Parse {
        /// The file.
        path: PathBuf,
        /// Parser message.
        message: String,
    },
    /// The assembled scene failed migration or validation.
    #[error("level_invalid: {0}")]
    Invalid(#[from] SceneFormatError),
    /// A file could not be read or written. On a failed save `written` lists what was already
    /// written; nothing was deleted.
    #[error("level_io_error: {path}: {source} (already written: {written:?})")]
    Io {
        /// The path that failed.
        path: PathBuf,
        /// The IO error.
        source: std::io::Error,
        /// Files written before the failure.
        written: Vec<PathBuf>,
    },
}

impl LevelDirError {
    /// The lowercase reason string of spec 37 § 9 (plus `level_parse_error`, `level_invalid`
    /// and `level_io_error`, documented on the book page).
    pub fn reason(&self) -> &'static str {
        match self {
            Self::AmbiguousLevelForm { .. } => "ambiguous_level_form",
            Self::NotFound(_) => "level_not_found",
            Self::EntityFileMismatch { .. } => "entity_file_mismatch",
            Self::DuplicateEntity { .. } => "duplicate_entity",
            Self::MixedSceneVersions(_) => "mixed_scene_versions",
            Self::UnsupportedVersion { .. } => "unsupported_version",
            Self::Parse { .. } => "level_parse_error",
            Self::Invalid(SceneFormatError::UnsupportedVersion { .. }) => "unsupported_version",
            Self::Invalid(_) => "level_invalid",
            Self::Io { .. } => "level_io_error",
        }
    }

    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
            written: Vec::new(),
        }
    }
}

/// The 32 lowercase hex digits of a guid (the entity file stem).
pub fn guid_hex(guid: Guid) -> String {
    format!("{:032x}", guid.as_u128())
}

/// The relative path of an entity's file inside a level directory:
/// `entities/<first two hex digits>/<32 hex>.entity.ron`.
pub fn entity_rel_path(guid: Guid) -> PathBuf {
    let hex = guid_hex(guid);
    Path::new(ENTITIES_DIR)
        .join(&hex[..2])
        .join(format!("{hex}{ENTITY_FILE_SUFFIX}"))
}

/// The two sibling paths of a level reference: `(x.level.ron, x.level.d)`, given either one (or
/// the bare stem `x`).
pub fn level_paths(path: &Path) -> (PathBuf, PathBuf) {
    let text = path.to_string_lossy();
    let text = text.trim_end_matches(['/', '\\']);
    let stem = text
        .strip_suffix(LEVEL_FILE_SUFFIX)
        .or_else(|| text.strip_suffix(LEVEL_DIR_SUFFIX))
        .unwrap_or(text);
    (
        PathBuf::from(format!("{stem}{LEVEL_FILE_SUFFIX}")),
        PathBuf::from(format!("{stem}{LEVEL_DIR_SUFFIX}")),
    )
}

/// Which form the level at `path` (either spelling) is stored in. Both existing is
/// `ambiguous_level_form`; neither is `level_not_found`.
pub fn level_form(path: &Path) -> Result<LevelForm, LevelDirError> {
    let (file, dir) = level_paths(path);
    match (file.is_file(), dir.is_dir()) {
        (true, true) => Err(LevelDirError::AmbiguousLevelForm { file, dir }),
        (true, false) => Ok(LevelForm::File),
        (false, true) => Ok(LevelForm::Dir),
        (false, false) => Err(LevelDirError::NotFound(path.to_path_buf())),
    }
}

/// The level directory `path` is inside of (or is), for a watcher that reports the files in it.
#[must_use]
pub fn containing_level_dir(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(LEVEL_DIR_SUFFIX))
        })
        .map(Path::to_path_buf)
}

/// Reads a level in either form and returns the assembled [`SceneFile`] with entities sorted by
/// guid: the cook input, identical for both forms (spec 37 § 3.6).
pub fn read_level_source(path: &Path) -> Result<SceneFile, LevelDirError> {
    let mut scene = read_level_as_stored(path)?;
    scene.entities.sort_by_key(|e| e.guid);
    Ok(scene)
}

/// Reads a level in either form. A file keeps its record order, for what spawns from it as it did
/// from the file; a directory, which has no order, is in guid order. The level's layer files
/// (`<level>.<layer>.layer.ron`, [`crate::layer_file`]) are folded back in.
pub fn read_level_as_stored(path: &Path) -> Result<SceneFile, LevelDirError> {
    let (file, dir) = level_paths(path);
    let mut scene = match level_form(path)? {
        LevelForm::Dir => read_level_dir(&dir)?.0,
        LevelForm::File => {
            let text = std::fs::read_to_string(&file).map_err(|e| LevelDirError::io(&file, e))?;
            SceneFile::from_ron_str(&text)?
        }
    };
    crate::layer_file::merge_layer_files_of(&file, &mut scene)?;
    Ok(scene)
}

/// Canonical text of one entity file (`EntityFile(` + the canonical pretty RON of `SceneFile`).
pub fn entity_file_text(entity: &SceneEntity) -> Result<String, LevelDirError> {
    let wrapper = EntityFile {
        format_version: LEVEL_DIR_FORMAT_VERSION,
        scene_format_version: SCENE_FORMAT_VERSION,
        entity: entity.clone(),
    };
    named_ron("EntityFile", &wrapper)
}

/// Canonical text of the header for `scene`'s resources.
pub fn header_text(scene: &SceneFile) -> Result<String, LevelDirError> {
    let header = LevelDirFile {
        format_version: LEVEL_DIR_FORMAT_VERSION,
        scene_format_version: SCENE_FORMAT_VERSION,
        resources: scene.resources.clone(),
    };
    named_ron("LevelDirFile", &header)
}

fn named_ron<T: Serialize>(name: &str, value: &T) -> Result<String, LevelDirError> {
    let mut text = ron::ser::to_string_pretty(value, regia_ecs::serialize::canonical_pretty_config())
        .map_err(SceneFormatError::from)?;
    text.push('\n');
    if let Some(rest) = text.strip_prefix('(') {
        text = format!("{name}({rest}");
    }
    Ok(text)
}

/// Only the two version fields, read when a full parse fails (a newer file whose body no longer
/// matches the current struct must still report `unsupported_version`). Missing fields read as 0.
fn peek_versions(text: &str) -> Option<(u32, u32)> {
    let ron::Value::Map(map) = ron::from_str::<ron::Value>(text).ok()? else {
        return None;
    };
    let field = |name: &str| {
        map.get(&ron::Value::String(name.to_owned()))
            .and_then(|v| v.clone().into_rust::<u32>().ok())
            .unwrap_or(0)
    };
    Some((field("format_version"), field("scene_format_version")))
}

/// [`check_versions`] for versions read by a full parse: both formats start at 1, so a zero
/// version word is damage, not a current file (audit-v035-formats 11). (A peek reads a missing
/// field as 0 and only reports newer versions.)
fn check_parsed_versions(path: &Path, format_version: u32, scene_version: u32) -> Result<(), LevelDirError> {
    if format_version == 0 {
        return Err(LevelDirError::UnsupportedVersion {
            path: path.to_path_buf(),
            found: 0,
            supported: LEVEL_DIR_FORMAT_VERSION,
        });
    }
    if scene_version == 0 {
        return Err(LevelDirError::UnsupportedVersion {
            path: path.to_path_buf(),
            found: 0,
            supported: SCENE_FORMAT_VERSION,
        });
    }
    check_versions(path, format_version, scene_version)
}

fn check_versions(path: &Path, format_version: u32, scene_version: u32) -> Result<(), LevelDirError> {
    if format_version > LEVEL_DIR_FORMAT_VERSION {
        return Err(LevelDirError::UnsupportedVersion {
            path: path.to_path_buf(),
            found: format_version,
            supported: LEVEL_DIR_FORMAT_VERSION,
        });
    }
    if scene_version > SCENE_FORMAT_VERSION {
        return Err(LevelDirError::UnsupportedVersion {
            path: path.to_path_buf(),
            found: scene_version,
            supported: SCENE_FORMAT_VERSION,
        });
    }
    Ok(())
}

fn parse_versioned<T: for<'de> Deserialize<'de>>(path: &Path, text: &str) -> Result<T, LevelDirError> {
    match ron::from_str::<T>(text) {
        Ok(value) => Ok(value),
        Err(err) => {
            if let Some((format_version, scene_version)) = peek_versions(text) {
                check_versions(path, format_version, scene_version)?;
            }
            Err(LevelDirError::Parse {
                path: path.to_path_buf(),
                message: err.to_string(),
            })
        }
    }
}

/// Parses the text of one entity file (exposed for the fuzz target: random bytes must give `Err`,
/// never a panic). Checks the versions; does not check the file name.
pub fn parse_entity_file(path: &Path, text: &str) -> Result<EntityFile, LevelDirError> {
    let file: EntityFile = parse_versioned(path, text)?;
    check_parsed_versions(path, file.format_version, file.scene_format_version)?;
    Ok(file)
}

/// Parses a header file and checks its versions.
pub fn parse_header(path: &Path, text: &str) -> Result<LevelDirFile, LevelDirError> {
    let header: LevelDirFile = parse_versioned(path, text)?;
    check_parsed_versions(path, header.format_version, header.scene_format_version)?;
    Ok(header)
}

/// Lists the entity files of a level directory (sorted), plus warnings for anything else found
/// under `entities/`.
fn list_entity_files(root: &Path) -> Result<(Vec<PathBuf>, Vec<String>), LevelDirError> {
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    let entities = root.join(ENTITIES_DIR);
    if !entities.is_dir() {
        return Ok((files, warnings));
    }
    let read = |dir: &Path| -> Result<Vec<std::fs::DirEntry>, LevelDirError> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).map_err(|e| LevelDirError::io(dir, e))? {
            out.push(entry.map_err(|e| LevelDirError::io(dir, e))?);
        }
        out.sort_by_key(std::fs::DirEntry::file_name);
        Ok(out)
    };
    for shard in read(&entities)? {
        let shard_path = shard.path();
        let name = shard.file_name().to_string_lossy().into_owned();
        let is_shard = shard_path.is_dir() && name.len() == 2 && name.bytes().all(|b| b.is_ascii_hexdigit());
        if !is_shard {
            warnings.push(format!(
                "ignored: {} is not a shard directory",
                shard_path.display()
            ));
            continue;
        }
        for file in read(&shard_path)? {
            let path = file.path();
            let file_name = file.file_name().to_string_lossy().into_owned();
            if path.is_file() && file_name.ends_with(ENTITY_FILE_SUFFIX) && !file_name.starts_with('.') {
                files.push(path);
            } else {
                warnings.push(format!("ignored: {} is not an entity file", path.display()));
            }
        }
    }
    Ok((files, warnings))
}

/// One parsed entity file with its path.
struct ReadEntity {
    path: PathBuf,
    file: EntityFile,
}

fn read_entity(path: &Path) -> Result<ReadEntity, LevelDirError> {
    let text = std::fs::read_to_string(path).map_err(|e| LevelDirError::io(path, e))?;
    let file = parse_entity_file(path, &text)?;
    Ok(ReadEntity {
        path: path.to_path_buf(),
        file,
    })
}

/// Parses `paths`, in parallel (scoped threads) above [`PARALLEL_READ_THRESHOLD`] files. The
/// result keeps the order of `paths`, so it is independent of scheduling.
fn read_entities(paths: &[PathBuf]) -> Result<Vec<ReadEntity>, LevelDirError> {
    if paths.len() <= PARALLEL_READ_THRESHOLD {
        return paths.iter().map(|p| read_entity(p)).collect();
    }
    let workers = std::thread::available_parallelism()
        .map_or(4, std::num::NonZeroUsize::get)
        .clamp(1, 16);
    let chunk = paths.len().div_ceil(workers);
    let results: Vec<Result<Vec<ReadEntity>, LevelDirError>> = std::thread::scope(|scope| {
        let handles: Vec<_> = paths
            .chunks(chunk)
            .map(|part| scope.spawn(move || part.iter().map(|p| read_entity(p)).collect()))
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join().unwrap_or_else(|_| {
                    Err(LevelDirError::Parse {
                        path: PathBuf::new(),
                        message: "entity reader thread panicked".into(),
                    })
                })
            })
            .collect()
    });
    let mut out = Vec::with_capacity(paths.len());
    for part in results {
        out.extend(part?);
    }
    Ok(out)
}

/// Checks file names, shards and duplicates; returns the entity records sorted by guid.
fn validate_entities(
    entities: Vec<ReadEntity>,
    warnings: &mut Vec<String>,
) -> Result<Vec<(PathBuf, EntityFile)>, LevelDirError> {
    let mut by_guid: BTreeMap<Guid, (PathBuf, EntityFile)> = BTreeMap::new();
    for ReadEntity { path, file } in entities {
        let guid = file.entity.guid;
        let stem = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let stem = stem.trim_end_matches(ENTITY_FILE_SUFFIX);
        if stem.len() != 32 || stem != guid_hex(guid) {
            return Err(LevelDirError::EntityFileMismatch { path, guid });
        }
        let shard = path
            .parent()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if shard != stem[..2] {
            warnings.push(format!(
                "entity_wrong_shard: {} belongs in shard {} (fixed on the next save)",
                path.display(),
                &stem[..2]
            ));
        }
        if let Some((first, _)) = by_guid.get(&guid) {
            return Err(LevelDirError::DuplicateEntity {
                guid,
                first: first.clone(),
                second: path,
            });
        }
        by_guid.insert(guid, (path, file));
    }
    Ok(by_guid.into_values().collect())
}

/// A level directory read without migrating: header, entity files by guid, report.
type RawLevelDir = (LevelDirFile, Vec<(PathBuf, EntityFile)>, LevelDirReport);

/// Reads the header and entity files of `root` without migrating: `(header, entities sorted by
/// guid, report)`.
fn read_raw(root: &Path) -> Result<RawLevelDir, LevelDirError> {
    let header_path = root.join(HEADER_FILE);
    if !header_path.is_file() {
        return Err(LevelDirError::NotFound(header_path));
    }
    let text = std::fs::read_to_string(&header_path).map_err(|e| LevelDirError::io(&header_path, e))?;
    let header = parse_header(&header_path, &text)?;
    let (paths, mut warnings) = list_entity_files(root)?;
    let entities = validate_entities(read_entities(&paths)?, &mut warnings)?;
    let report = LevelDirReport {
        entities: entities.len(),
        files_read: paths.len() + 1,
        warnings,
    };
    Ok((header, entities, report))
}

/// Reads a level directory into an in-memory [`SceneFile`] (entities sorted by guid) plus the
/// report of what was read. Every file must declare the same scene version
/// (`mixed_scene_versions` otherwise); the assembled file is migrated once and validated.
pub fn read_level_dir(root: &Path) -> Result<(SceneFile, LevelDirReport), LevelDirError> {
    let _span = tracing::info_span!("level_dir_read", root = %root.display()).entered();
    let (header, entities, report) = read_raw(root)?;
    let version = header.scene_format_version;
    if let Some((path, file)) = entities.iter().find(|(_, f)| f.scene_format_version != version) {
        return Err(LevelDirError::MixedSceneVersions(format!(
            "header is v{version}, {} is v{}",
            path.display(),
            file.scene_format_version
        )));
    }
    let mut scene = SceneFile {
        format_version: version,
        entities: entities.into_iter().map(|(_, f)| f.entity).collect(),
        resources: header.resources,
    };
    scene.finish_load()?;
    Ok((scene, report))
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes `bytes` to `path` atomically: a temp file in the same directory, `fsync`, rename.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Writes `scene` as a level directory, touching only files whose bytes changed. Entity files
/// first (guid order, each atomic), then the header (the last write), then deletions of files
/// whose entity is gone and their empty shard directories, only after every write, the header's
/// included, succeeded (spec 37 § 3.4: a failed write deletes nothing).
pub fn write_level_dir(root: &Path, scene: &SceneFile) -> Result<LevelDirWriteReport, LevelDirError> {
    write_level_dir_impl(root, scene, None)
}

/// Like [`write_level_dir`], but only the entities in `dirty` (and the header when
/// `dirty.header`) are considered: the editor's save of its dirty set. A dirty guid absent from
/// `scene` has its file deleted. The byte comparison still applies as a safety net.
pub fn write_level_dir_dirty(
    root: &Path,
    scene: &SceneFile,
    dirty: &LevelDirtySet,
) -> Result<LevelDirWriteReport, LevelDirError> {
    write_level_dir_impl(root, scene, Some(dirty))
}

fn write_level_dir_impl(
    root: &Path,
    scene: &SceneFile,
    dirty: Option<&LevelDirtySet>,
) -> Result<LevelDirWriteReport, LevelDirError> {
    let _span =
        tracing::info_span!("level_dir_write", root = %root.display(), entities = scene.entities.len())
            .entered();
    let mut report = LevelDirWriteReport::default();
    let mut ordered: Vec<&SceneEntity> = scene.entities.iter().collect();
    ordered.sort_by_key(|e| e.guid);
    let wanted: BTreeSet<PathBuf> = ordered
        .iter()
        .map(|e| root.join(entity_rel_path(e.guid)))
        .collect();
    let fail = |path: &Path, source: std::io::Error, written: &[PathBuf]| LevelDirError::Io {
        path: path.to_path_buf(),
        source,
        written: written.to_vec(),
    };

    for entity in &ordered {
        if dirty.is_some_and(|d| !d.entities.contains(&entity.guid)) {
            continue;
        }
        let path = root.join(entity_rel_path(entity.guid));
        let text = entity_file_text(entity)?;
        if std::fs::read(&path).is_ok_and(|old| old == text.as_bytes()) {
            report.unchanged += 1;
            continue;
        }
        write_atomic(&path, text.as_bytes()).map_err(|e| fail(&path, e, &report.written))?;
        report.written.push(path);
    }

    // The header is the last write, and it comes before any deletion: a failed header write must
    // leave every file in place, as a failed entity write does.
    if dirty.is_none_or(|d| d.header) {
        let path = root.join(HEADER_FILE);
        let text = header_text(scene)?;
        if std::fs::read(&path).is_ok_and(|old| old == text.as_bytes()) {
            report.unchanged += 1;
        } else {
            write_atomic(&path, text.as_bytes()).map_err(|e| fail(&path, e, &report.written))?;
            report.written.push(path);
        }
    }

    // Deletions: entity files on disk that the scene no longer wants at that path.
    let (existing, _) = list_entity_files(root).map_err(|e| match e {
        LevelDirError::Io { path, source, .. } => fail(&path, source, &report.written),
        other => other,
    })?;
    for path in existing {
        if wanted.contains(&path) {
            continue;
        }
        if let Some(dirty) = dirty {
            let guid = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(ENTITY_FILE_SUFFIX))
                .and_then(|hex| u128::from_str_radix(hex, 16).ok())
                .map(Guid::from_u128);
            if !guid.is_some_and(|g| dirty.entities.contains(&g)) {
                continue;
            }
        }
        std::fs::remove_file(&path).map_err(|e| fail(&path, e, &report.written))?;
        if let Some(shard) = path.parent()
            && std::fs::read_dir(shard).is_ok_and(|mut it| it.next().is_none())
        {
            let _ = std::fs::remove_dir(shard);
        }
        report.deleted.push(path);
    }

    Ok(report)
}

/// Converts a single-file level to the directory form (lossless; the scene is migrated to the
/// current version on the way).
pub fn split_level_file(level_file: &Path, out_dir: &Path) -> Result<LevelDirWriteReport, LevelDirError> {
    let text = std::fs::read_to_string(level_file).map_err(|e| LevelDirError::io(level_file, e))?;
    let scene = SceneFile::from_ron_str(&text)?;
    write_level_dir(out_dir, &scene)
}

/// Converts a level directory to the single-file form (entities in guid order), written
/// atomically.
pub fn merge_level_dir(root: &Path, out_file: &Path) -> Result<(), LevelDirError> {
    let (scene, _) = read_level_dir(root)?;
    let text = scene.to_ron_string()?;
    write_atomic(out_file, text.as_bytes()).map_err(|e| LevelDirError::io(out_file, e))
}

/// The oldest and newest versions found in a level directory: `(min dir format, min scene
/// format, max scene format)`. `regia upgrade-project` plans with it.
pub fn level_dir_versions(root: &Path) -> Result<(u32, u32, u32), LevelDirError> {
    let (header, entities, _) = read_raw(root)?;
    let mut dir_min = header.format_version;
    let mut scene_min = header.scene_format_version;
    let mut scene_max = header.scene_format_version;
    for (_, file) in &entities {
        dir_min = dir_min.min(file.format_version);
        scene_min = scene_min.min(file.scene_format_version);
        scene_max = scene_max.max(file.scene_format_version);
    }
    Ok((dir_min, scene_min, scene_max))
}

/// Rewrites every file of a level directory at the latest versions, keeping the directory form.
/// Unlike [`read_level_dir`] it accepts mixed scene versions: records are migrated in groups of
/// equal version (the header's resources with the header's group), then reassembled and
/// validated.
pub fn upgrade_level_dir(root: &Path) -> Result<LevelDirWriteReport, LevelDirError> {
    let (header, entities, _) = read_raw(root)?;
    let mut groups: BTreeMap<u32, Vec<SceneEntity>> = BTreeMap::new();
    for (_, file) in entities {
        groups
            .entry(file.scene_format_version)
            .or_default()
            .push(file.entity);
    }
    groups.entry(header.scene_format_version).or_default();
    let mut resources = header.resources;
    let mut all = Vec::new();
    for (version, group) in groups {
        let mut part = SceneFile {
            format_version: version,
            entities: group,
            resources: if version == header.scene_format_version {
                std::mem::take(&mut resources)
            } else {
                BTreeMap::new()
            },
        };
        part.normalize_and_migrate()?;
        all.extend(part.entities);
        if version == header.scene_format_version {
            resources = part.resources;
        }
    }
    all.sort_by_key(|e| e.guid);
    let scene = SceneFile {
        format_version: SCENE_FORMAT_VERSION,
        entities: all,
        resources,
    };
    scene.validate()?;
    write_level_dir(root, &scene)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-8861: only `.<name>.<pid>.stage` names are upgrade staging directories.
    #[test]
    fn upgrade_stage_dir_names() {
        for yes in [".hall.level.d.4242.stage", ".a.1.stage"] {
            assert!(is_upgrade_stage_dir_name(yes), "{yes}");
        }
        for no in [
            "hall.level.d",
            "hall.level.d.4242.stage",
            ".hall.level.d.stage",
            ".hall.level.d.42x.stage",
            "..1.stage",
            ".stage",
            ".git",
        ] {
            assert!(!is_upgrade_stage_dir_name(no), "{no}");
        }
    }

    #[test]
    fn rel_path_shards_by_first_two_hex_digits() {
        let guid = Guid::from_u128(0x8f1c_3a00_0000_0000_0000_0000_0000_0001);
        let path = entity_rel_path(guid);
        assert_eq!(
            path,
            Path::new("entities/8f/8f1c3a00000000000000000000000001.entity.ron")
        );
        assert!(path.file_name().is_some_and(|n| n.len() <= 45));
    }

    /// audit-v035-formats 11: a header or entity file with a zero version word loaded as a
    /// current file (only newer versions were refused).
    #[test]
    fn zero_version_words_are_refused() {
        let path = Path::new("forest.level.ron");
        let header = |fv: u32, sv: u32| {
            format!("LevelDirFile(format_version: {fv}, scene_format_version: {sv}, resources: {{}})")
        };
        let current = header(LEVEL_DIR_FORMAT_VERSION, SCENE_FORMAT_VERSION);
        parse_header(path, &current).expect("a current header loads");
        assert!(parse_header(path, &header(0, SCENE_FORMAT_VERSION)).is_err());
        assert!(parse_header(path, &header(LEVEL_DIR_FORMAT_VERSION, 0)).is_err());
    }

    #[test]
    fn level_paths_accept_either_spelling() {
        let (f, d) = level_paths(Path::new("a/forest.level.d/"));
        assert_eq!(f, Path::new("a/forest.level.ron"));
        assert_eq!(d, Path::new("a/forest.level.d"));
        let (f2, d2) = level_paths(Path::new("a/forest.level.ron"));
        assert_eq!((f, d), (f2, d2));
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut state = 0x1234_5678_u64;
        for len in 0..200usize {
            let bytes: Vec<u8> = (0..len)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    (state >> 24) as u8
                })
                .collect();
            let text = String::from_utf8_lossy(&bytes);
            assert!(parse_entity_file(Path::new("x.entity.ron"), &text).is_err());
        }
    }
}
