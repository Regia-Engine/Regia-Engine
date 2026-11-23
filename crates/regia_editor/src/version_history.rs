// SPDX-License-Identifier: MPL-2.0
//! Version history of levels and asset documents (ADR-7928, `docs/specs/19-editor.md` § 7,
//! `book/agents/editor-autosave.md` "Version history").
//!
//! **Store.** Every autosave of a level or a document and every manual save of one keeps a copy in
//! `<project>/.regia/history/<target id>/`: `target.ron` says what the folder holds (kind, original
//! path, label) and each version is one file `<unix ms>-<seq>-<source>.version` holding the bytes
//! exactly (a level version is its single-file `.level.ron` text, also for a `.level.d` level). The
//! folder is editor state under `.regia/`, never a content format: the cook and the packager read
//! `assets/` only.
//!
//! **Pruning.** Per target the newest `history.keep_autosaves` autosave versions and the newest
//! `history.keep_saves` save versions stay (the restore safety copies count as saves); then, while
//! the whole store is larger than `history.max_mb`, the oldest version anywhere goes. A copy whose
//! bytes equal the newest version of the same target and source is not written again.
//!
//! **Writers.** The autosave worker ([`crate::doc_autosave`]) writes every version, so the level
//! autosave, a document autosave and a manual save never race on one folder; without a worker the
//! caller writes inline.
//!
//! **Restore.** A level version replaces the open level's contents as one undo entry
//! ([`RestoreLevelVersion`]); a document version opens in its editor as unsaved edits through the
//! autosave registry's restore hook, also as one undo entry ([`RestoreDocVersion`]), after the
//! document's current state is kept as a `before_restore` version. Nothing on disk is overwritten
//! until the user saves. The agent surface is `editor.versions`, `editor.version_preview` and
//! `editor.version_restore` ([`methods`]); `editor.duplicate_project` copies a project
//! ([`crate::project::duplicate_project`]).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::SystemTime;

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::atomic_write::write_atomic;
use crate::doc_autosave::DocKey;
use crate::viewport_agent::Handler;

/// The history folder under the project state directory.
pub const HISTORY_DIR: &str = "history";
/// The file in each target folder that names the target.
pub const TARGET_FILE: &str = "target.ron";
/// The extension of one version's bytes.
pub const VERSION_EXTENSION: &str = "version";
/// The target kind of a level.
pub const LEVEL_KIND: &str = "level";
/// The most text [`preview`] returns.
pub const PREVIEW_LIMIT: usize = 64 * 1024;

/// The preference keys of the retention policy ([`HistoryPolicy::from_prefs`]).
pub const PREF_KEEP_AUTOSAVES: &str = "history.keep_autosaves";
/// Manual saves kept per level and document.
pub const PREF_KEEP_SAVES: &str = "history.keep_saves";
/// The size cap of the whole store, MiB.
pub const PREF_MAX_MB: &str = "history.max_mb";

/// `<project>/.regia/history/`.
#[must_use]
pub fn history_dir(project_root: &Path) -> PathBuf {
    regia_core::brand::project_state_dir(project_root).join(HISTORY_DIR)
}

/// What wrote a version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VersionSource {
    /// The periodic autosave (or `editor.autosave_now`).
    Autosave,
    /// A save the user (or an agent) asked for.
    Save,
    /// The document's state just before a version was restored over it.
    BeforeRestore,
}

impl VersionSource {
    /// The name in file names and the agent protocol: `autosave`, `save`, `before_restore`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Autosave => "autosave",
            Self::Save => "save",
            Self::BeforeRestore => "before_restore",
        }
    }

    /// Parses [`Self::as_str`].
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "autosave" => Some(Self::Autosave),
            "save" => Some(Self::Save),
            "before_restore" => Some(Self::BeforeRestore),
            _ => None,
        }
    }

    /// What the Version History window shows.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Autosave => "Autosave",
            Self::Save => "Save",
            Self::BeforeRestore => "Before restore",
        }
    }
}

/// Which level or document a version belongs to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct VersionTarget {
    /// [`LEVEL_KIND`] or the document's autosave kind (`matgraph`, `ui_design`, ...).
    pub kind: String,
    /// The original file, `/`-separated, project-relative when under the project.
    pub original: Option<String>,
    /// What the window shows (the file name, or `untitled <kind>`).
    pub label: String,
}

/// `path` relative to the project, `/`-separated.
fn stored_path(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

impl VersionTarget {
    /// The target of a level saved at `path` (`None` while untitled).
    #[must_use]
    pub fn level(project_root: &Path, path: Option<&Path>) -> Self {
        Self {
            kind: LEVEL_KIND.to_string(),
            original: path.map(|p| stored_path(project_root, p)),
            label: path.and_then(Path::file_name).map_or_else(
                || "untitled level".to_string(),
                |n| n.to_string_lossy().into_owned(),
            ),
        }
    }

    /// The target of an autosaved document.
    #[must_use]
    pub fn doc(project_root: &Path, key: &DocKey) -> Self {
        Self {
            kind: key.kind.clone(),
            original: key.original.as_deref().map(|p| stored_path(project_root, p)),
            label: key.label.clone(),
        }
    }

    /// The folder name: FNV-1a 64 of the kind and the original path (or the label while untitled),
    /// 16 lowercase hex digits.
    #[must_use]
    pub fn id(&self) -> String {
        let mut text = self.kind.clone();
        text.push('\0');
        match &self.original {
            Some(path) => text.push_str(path),
            None => {
                text.push('\u{1}');
                text.push_str(&self.label);
            }
        }
        let hash = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
        format!("{hash:016x}")
    }

    /// The original as an absolute path.
    #[must_use]
    pub fn original_path(&self, project_root: &Path) -> Option<PathBuf> {
        self.original.as_deref().map(|text| {
            let path = Path::new(text);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                project_root.join(path)
            }
        })
    }
}

/// How many versions stay and how large the store may grow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryPolicy {
    /// Autosave versions kept per target (0: autosaves keep no versions).
    pub keep_autosaves: usize,
    /// Save versions kept per target (0: saves keep no versions).
    pub keep_saves: usize,
    /// The size cap of the whole store, bytes (0: no cap).
    pub max_bytes: u64,
}

impl Default for HistoryPolicy {
    fn default() -> Self {
        Self {
            keep_autosaves: 10,
            keep_saves: 10,
            max_bytes: 512 * 1024 * 1024,
        }
    }
}

impl HistoryPolicy {
    /// The policy the preferences ask for.
    #[must_use]
    pub fn from_prefs(prefs: &crate::editor_prefs::EditorPreferences) -> Self {
        let count = |key| usize::try_from(prefs.int(key).max(0)).unwrap_or(0);
        Self {
            keep_autosaves: count(PREF_KEEP_AUTOSAVES),
            keep_saves: count(PREF_KEEP_SAVES),
            max_bytes: u64::try_from(prefs.int(PREF_MAX_MB).max(0))
                .unwrap_or(0)
                .saturating_mul(1024 * 1024),
        }
    }

    /// The policy of the session: the preferences when the world has them, else the defaults.
    #[must_use]
    pub fn of_world(world: &World) -> Self {
        world
            .get_resource::<crate::editor_prefs::EditorPreferences>()
            .map_or_else(Self::default, Self::from_prefs)
    }

    /// How many versions of `source` stay per target.
    #[must_use]
    pub fn keep(&self, source: VersionSource) -> usize {
        match source {
            VersionSource::Autosave => self.keep_autosaves,
            VersionSource::Save | VersionSource::BeforeRestore => self.keep_saves,
        }
    }
}

/// One kept version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// `<target id>/<file stem>`: what `editor.version_preview` and `editor.version_restore` take.
    pub id: String,
    /// What it is a version of.
    pub target: VersionTarget,
    /// What wrote it.
    pub source: VersionSource,
    /// When, Unix milliseconds.
    pub saved_unix_ms: u64,
    /// Bytes.
    pub size: u64,
    /// The file.
    pub file: PathBuf,
}

/// The bytes of one version to record: given, or read from a file by the writer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionBytes {
    /// These bytes.
    Bytes(Vec<u8>),
    /// The bytes of this file when the writer runs (a manual save's file).
    File(PathBuf),
}

/// One version a writer records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionWrite {
    /// The target.
    pub target: VersionTarget,
    /// What wrote it.
    pub source: VersionSource,
    /// The bytes.
    pub bytes: VersionBytes,
}

fn unix_ms(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Whether `id` is a target folder name ([`VersionTarget::id`]).
fn is_target_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Parses a version file stem `<ms>-<seq>-<source>`.
fn parse_stem(stem: &str) -> Option<(u64, u32, VersionSource)> {
    let mut parts = stem.splitn(3, '-');
    let ms = parts.next()?;
    let seq = parts.next()?;
    let source = parts.next()?;
    if ms.is_empty() || !ms.bytes().all(|b| b.is_ascii_digit()) || !seq.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((ms.parse().ok()?, seq.parse().ok()?, VersionSource::parse(source)?))
}

/// The write sequence number in a version's file stem (0 when the name does not parse).
fn write_seq(version: &Version) -> u32 {
    version
        .file
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(parse_stem)
        .map_or(0, |(_, seq, _)| seq)
}

/// Reads one target folder's `target.ron`.
fn read_target(dir: &Path) -> Option<VersionTarget> {
    let text = std::fs::read_to_string(dir.join(TARGET_FILE)).ok()?;
    ron::from_str(&text).ok()
}

/// The versions in one target folder, newest first.
fn versions_in(dir: &Path, target: &VersionTarget, target_id: &str) -> Vec<Version> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(u64, u32, Version)> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some(VERSION_EXTENSION))
        .filter_map(|file| {
            let stem = file.file_stem()?.to_str()?.to_string();
            let (ms, seq, source) = parse_stem(&stem)?;
            let size = std::fs::metadata(&file).ok()?.len();
            Some((
                ms,
                seq,
                Version {
                    id: format!("{target_id}/{stem}"),
                    target: target.clone(),
                    source,
                    saved_unix_ms: ms,
                    size,
                    file,
                },
            ))
        })
        .collect();
    out.sort_by_key(|(ms, seq, _)| std::cmp::Reverse((*ms, *seq)));
    out.into_iter().map(|(_, _, v)| v).collect()
}

/// Tests: how many times [`targets`] read each project's history folder (review A4: the window
/// must not re-read the store every frame).
#[cfg(test)]
static LIST_READS: std::sync::Mutex<std::collections::BTreeMap<PathBuf, u64>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Tests: the number of [`targets`] reads of `project_root`'s store so far.
#[cfg(test)]
fn list_reads(project_root: &Path) -> u64 {
    LIST_READS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(project_root)
        .copied()
        .unwrap_or(0)
}

/// Each project's store generation in this process, keyed by history folder (review A4,
/// ADR-8435): [`record`] bumps it after a version is written and [`prune`] after one is removed,
/// so the Version History window re-reads the store only when it changed.
static GENERATIONS: std::sync::Mutex<std::collections::BTreeMap<PathBuf, u64>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Marks `project_root`'s store as changed.
fn bump_generation(project_root: &Path) {
    let mut generations = GENERATIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let generation = generations.entry(history_dir(project_root)).or_insert(0);
    *generation = generation.wrapping_add(1);
}

/// How many times this process has changed `project_root`'s store (a version written or pruned).
/// A reader that saw the same number before has nothing new to read, unless another process wrote.
#[must_use]
pub fn store_generation(project_root: &Path) -> u64 {
    GENERATIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&history_dir(project_root))
        .copied()
        .unwrap_or(0)
}

/// Every target with at least one version, by label.
#[must_use]
pub fn targets(project_root: &Path) -> Vec<(String, VersionTarget)> {
    #[cfg(test)]
    {
        *LIST_READS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(project_root.to_path_buf())
            .or_insert(0) += 1;
    }
    let Ok(entries) = std::fs::read_dir(history_dir(project_root)) else {
        return Vec::new();
    };
    let mut out: Vec<(String, VersionTarget)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let id = e.file_name().to_str()?.to_string();
            if !is_target_id(&id) {
                return None;
            }
            let target = read_target(&e.path())?;
            (!versions_in(&e.path(), &target, &id).is_empty()).then_some((id, target))
        })
        .collect();
    out.sort_by(|a, b| {
        a.1.kind
            .cmp(&b.1.kind)
            .then_with(|| a.1.label.cmp(&b.1.label))
            .then_with(|| a.0.cmp(&b.0))
    });
    out
}

/// The versions of `target`, newest first.
#[must_use]
pub fn versions_of(project_root: &Path, target: &VersionTarget) -> Vec<Version> {
    let id = target.id();
    versions_in(&history_dir(project_root).join(&id), target, &id)
}

/// Every version in the store, newest first.
#[must_use]
pub fn all_versions(project_root: &Path) -> Vec<Version> {
    let mut out: Vec<Version> = targets(project_root)
        .into_iter()
        .flat_map(|(_, target)| versions_of(project_root, &target))
        .collect();
    out.sort_by(|a, b| {
        b.saved_unix_ms
            .cmp(&a.saved_unix_ms)
            .then_with(|| b.id.cmp(&a.id))
    });
    out
}

/// The version `id` (`<target id>/<file stem>`). An id of any other shape names no file.
#[must_use]
pub fn find(project_root: &Path, id: &str) -> Option<Version> {
    let (target_id, stem) = id.split_once('/')?;
    if !is_target_id(target_id) || parse_stem(stem).is_none() {
        return None;
    }
    let dir = history_dir(project_root).join(target_id);
    let target = read_target(&dir)?;
    versions_in(&dir, &target, target_id)
        .into_iter()
        .find(|v| v.id == id)
}

/// Gives every version a unique name inside one process (two in one millisecond).
static NEXT_SEQ: AtomicU32 = AtomicU32::new(0);

/// Records one version and prunes. `Ok(None)` when the policy keeps no versions of that source or
/// the bytes equal the newest version of the same target and source.
///
/// # Errors
///
/// The bytes could not be read (a [`VersionBytes::File`]) or written.
pub fn record(
    project_root: &Path,
    write: &VersionWrite,
    policy: &HistoryPolicy,
) -> Result<Option<Version>, String> {
    if policy.keep(write.source) == 0 {
        return Ok(None);
    }
    let bytes = match &write.bytes {
        VersionBytes::Bytes(bytes) => bytes.clone(),
        VersionBytes::File(path) => {
            std::fs::read(path).map_err(|e| format!("version of {}: {e}", path.display()))?
        }
    };
    let target_id = write.target.id();
    let dir = history_dir(project_root).join(&target_id);
    if read_target(&dir).as_ref() != Some(&write.target) {
        let text = ron::ser::to_string_pretty(&write.target, regia_core::ron_pretty())
            .map_err(|e| format!("cannot encode the version target: {e}"))?;
        write_atomic(&dir.join(TARGET_FILE), text.as_bytes())?;
        bump_generation(project_root);
    }
    let same = versions_in(&dir, &write.target, &target_id)
        .into_iter()
        .find(|v| v.source == write.source)
        .filter(|newest| newest.size == bytes.len() as u64)
        .and_then(|newest| std::fs::read(&newest.file).ok())
        .is_some_and(|newest| newest == bytes);
    if same {
        return Ok(None);
    }
    let ms = unix_ms(SystemTime::now());
    let (stem, file) = loop {
        let seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed) % 1_000_000;
        let stem = format!("{ms:013}-{seq:06}-{}", write.source.as_str());
        let file = dir.join(format!("{stem}.{VERSION_EXTENSION}"));
        if !file.exists() {
            break (stem, file);
        }
    };
    write_atomic(&file, &bytes)?;
    bump_generation(project_root);
    let version = Version {
        id: format!("{target_id}/{stem}"),
        target: write.target.clone(),
        source: write.source,
        saved_unix_ms: ms,
        size: bytes.len() as u64,
        file,
    };
    prune(project_root, policy, Some(&version.file));
    Ok(Some(version))
}

/// What [`prune`] removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Versions removed.
    pub removed: usize,
    /// Bytes freed.
    pub freed: u64,
}

/// Prunes the store to `policy`, oldest first: per target the newest versions of each source the
/// policy keeps, then the oldest anywhere while the store is over the size cap. `keep` (the version
/// just written) is never removed; a target folder with no versions left goes.
pub fn prune(project_root: &Path, policy: &HistoryPolicy, keep: Option<&Path>) -> PruneReport {
    let mut report = PruneReport::default();
    let remove = |version: &Version, report: &mut PruneReport| {
        if keep == Some(version.file.as_path()) {
            return false;
        }
        match std::fs::remove_file(&version.file) {
            Ok(()) => {
                report.removed += 1;
                report.freed += version.size;
                true
            }
            // A second writer pruned it first.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(e) => {
                tracing::warn!(target: "regia_editor", "version history: {}: {e}", version.file.display());
                false
            }
        }
    };
    let root = history_dir(project_root);
    let mut left: Vec<Version> = Vec::new();
    for (id, target) in targets(project_root) {
        let mut counts = [0_usize; 2];
        for version in versions_in(&root.join(&id), &target, &id) {
            let slot = usize::from(version.source != VersionSource::Autosave);
            counts[slot] += 1;
            if counts[slot] > policy.keep(version.source) && remove(&version, &mut report) {
                continue;
            }
            left.push(version);
        }
    }
    if policy.max_bytes > 0 {
        // Oldest first across targets: versions written in the same millisecond keep their write
        // order through the stem's sequence number (the id alone would order them by target hash).
        left.sort_by(|a, b| {
            a.saved_unix_ms
                .cmp(&b.saved_unix_ms)
                .then_with(|| write_seq(a).cmp(&write_seq(b)))
                .then_with(|| a.id.cmp(&b.id))
        });
        let mut total: u64 = left.iter().map(|v| v.size).sum();
        for version in &left {
            if total <= policy.max_bytes {
                break;
            }
            if remove(version, &mut report) {
                total = total.saturating_sub(version.size);
            }
        }
    }
    // Folders whose last version went.
    if let Ok(entries) = std::fs::read_dir(&root) {
        for dir in entries.filter_map(Result::ok).map(|e| e.path()) {
            let empty = std::fs::read_dir(&dir).is_ok_and(|mut files| {
                files.all(|f| {
                    f.ok().is_some_and(|f| {
                        f.path().extension().and_then(|e| e.to_str()) != Some(VERSION_EXTENSION)
                    })
                })
            });
            if empty && dir.is_dir() {
                let _ = std::fs::remove_file(dir.join(TARGET_FILE));
                let _ = std::fs::remove_dir(&dir);
            }
        }
    }
    if report.removed > 0 {
        bump_generation(project_root);
        tracing::debug!(target: "regia_editor", "version history pruned {} version(s), {} bytes", report.removed, report.freed);
    }
    report
}

/// Total size of the store, bytes.
#[must_use]
pub fn store_size(project_root: &Path) -> u64 {
    all_versions(project_root).iter().map(|v| v.size).sum()
}

// ---------------------------------------------------------------------------------------------
// Writers: queue on the autosave worker, else inline.

/// Records `writes` on the autosave worker when the session has one, else here. Errors are logged
/// (a version is a convenience copy; it never fails a save).
pub fn queue(world: &mut World, project_root: &Path, writes: Vec<VersionWrite>) {
    if writes.is_empty() {
        return;
    }
    let policy = HistoryPolicy::of_world(world);
    if let Some(mut state) = world.get_resource_mut::<crate::doc_autosave::AutosaveState>() {
        state.queue_history(project_root, writes, policy);
        return;
    }
    for write in &writes {
        if let Err(err) = record(project_root, write, &policy) {
            tracing::warn!(target: "regia_editor", "version history: {err}");
        }
    }
}

/// The manual save of the open level to `path` keeps a `save` version (called by
/// [`crate::level_io::save_level_and_terrain`], the path every save a user asks for takes). A
/// `.level.d` level keeps its single-file text.
pub fn note_level_saved(world: &mut World, open: &crate::level_io::OpenLevel, path: &Path) {
    let Some(root) = crate::matgraph_editor::project_root(world) else {
        return;
    };
    let target = VersionTarget::level(&root, Some(path));
    let bytes = if crate::level_io::is_level_dir(path) {
        let snapshot = crate::level_io::snapshot_saved_level(world, open.id).map(|mut level| {
            crate::level_io::keep_layer_binding(world, open.id, &mut level.scene);
            level
        });
        match snapshot
            .map_err(|e| e.to_string())
            .and_then(|l| l.to_ron_string().map_err(|e| e.to_string()))
        {
            Ok(text) => VersionBytes::Bytes(text.into_bytes()),
            Err(err) => {
                tracing::warn!(target: "regia_editor", "version history: {err}");
                return;
            }
        }
    } else {
        VersionBytes::File(path.to_path_buf())
    };
    queue(
        world,
        &root,
        vec![VersionWrite {
            target,
            source: VersionSource::Save,
            bytes,
        }],
    );
}

/// How often the autosave tick compares the dirty documents for saves while the session owns no
/// document autosave (the registry's dirty lists are not free for every editor).
pub const SAVE_CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// The documents seen dirty at the last check (a document that turns clean was saved).
#[derive(Resource, Debug, Default, Clone)]
pub struct SeenDirtyDocs {
    /// Dirty documents at the last check, by [`DocKey::id`].
    pub docs: std::collections::BTreeMap<String, DocKey>,
    /// When the last check ran.
    pub checked: Option<std::time::Instant>,
}

/// Whether the save check is due at `now` ([`SAVE_CHECK_EVERY`] since the last).
#[must_use]
pub fn save_check_due(world: &World, now: std::time::Instant) -> bool {
    world
        .get_resource::<SeenDirtyDocs>()
        .and_then(|seen| seen.checked)
        .is_none_or(|last| now.saturating_duration_since(last) >= SAVE_CHECK_EVERY)
}

/// The autosave tick's save check: a document that was dirty at the last check and is clean now was
/// saved (or reverted); its file keeps a `save` version. A revert leaves the file as it was, so its
/// copy equals the newest save and is not written again.
pub fn note_doc_saves(world: &mut World, project_root: &Path, dirty_now: &[DocKey], at: std::time::Instant) {
    let now: std::collections::BTreeMap<String, DocKey> =
        dirty_now.iter().map(|key| (key.id(), key.clone())).collect();
    let mut seen = world.get_resource_or_insert_with(SeenDirtyDocs::default);
    seen.checked = Some(at);
    let before = std::mem::replace(&mut seen.docs, now);
    let seen = &world.resource::<SeenDirtyDocs>().docs;
    let writes: Vec<VersionWrite> = before
        .into_iter()
        .filter(|(id, _)| !seen.contains_key(id))
        .filter_map(|(_, key)| {
            let original = key.original.clone().filter(|p| p.is_file())?;
            Some(VersionWrite {
                target: VersionTarget::doc(project_root, &key),
                source: VersionSource::Save,
                bytes: VersionBytes::File(original),
            })
        })
        .collect();
    queue(world, project_root, writes);
}

// ---------------------------------------------------------------------------------------------
// Preview.

/// A read-only look at one version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionPreview {
    /// The version.
    pub version: Version,
    /// Its text (UTF-8), up to [`PREVIEW_LIMIT`] bytes; `None` for binary bytes.
    pub text: Option<String>,
    /// Whether the text was cut at [`PREVIEW_LIMIT`].
    pub truncated: bool,
    /// For a level: the names of its entities (an unnamed one as `(unnamed)`), in file order.
    pub entities: Option<Vec<String>>,
}

/// Reads `version` for a read-only preview (nothing in the world changes).
///
/// # Errors
///
/// The file cannot be read.
pub fn preview(version: &Version) -> Result<VersionPreview, String> {
    let bytes =
        std::fs::read(&version.file).map_err(|e| format!("cannot read {}: {e}", version.file.display()))?;
    let text = String::from_utf8(bytes).ok();
    let level_text = if version.target.kind == LEVEL_KIND {
        text.as_deref()
    } else {
        None
    };
    let entities = level_text
        .and_then(|t| regia_scene::prelude::SceneFile::from_ron_str(t).ok())
        .map(|scene| {
            scene
                .entities
                .iter()
                .map(|e| e.name.clone().unwrap_or_else(|| "(unnamed)".to_string()))
                .collect()
        });
    let (text, truncated) = match text {
        Some(mut text) if text.len() > PREVIEW_LIMIT => {
            let mut cut = PREVIEW_LIMIT;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            (Some(text), true)
        }
        other => (other, false),
    };
    Ok(VersionPreview {
        version: version.clone(),
        text,
        truncated,
        entities,
    })
}

// ---------------------------------------------------------------------------------------------
// Restore.

/// Why a restore was refused: an agent error code and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreError {
    /// `no_project`, `no_such_version`, `version_level_not_open`, `prefab_open`,
    /// `version_unreadable` or `version_no_editor`.
    pub code: &'static str,
    /// What happened.
    pub message: String,
}

impl RestoreError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// The entity of every saved record of `level`, by guid.
fn level_entities(
    world: &mut World,
    level: regia_scene::prelude::LevelId,
) -> Vec<(regia_core::id::Guid, Entity)> {
    let mut query = world.query::<(
        Entity,
        &regia_scene::prelude::EntityGuid,
        &regia_scene::prelude::LevelMember,
    )>();
    query
        .iter(world)
        .filter(|(_, _, member)| member.level() == level)
        .map(|(entity, guid, _)| (guid.0, entity))
        .collect()
}

/// Replaces the contents of the open level `level` with `contents`, keeping its id: older undo
/// entries that name an entity whose guid is in both find the new entity ([`crate::command`]'s
/// remap). The selection is cleared (its entities are gone).
fn replace_level(world: &mut World, contents: &regia_scene::prelude::Level) -> Result<(), String> {
    let id = contents.id;
    let old = level_entities(world, id);
    world
        .get_resource_or_insert_with::<regia_scene::prelude::LevelLibrary>(
            regia_scene::prelude::LevelLibrary::new,
        )
        .insert(contents.clone());
    regia_scene::level::open_level(world, id, regia_scene::prelude::LoadMode::Replace)
        .map_err(|e| e.to_string())?;
    let new: std::collections::HashMap<regia_core::id::Guid, Entity> =
        level_entities(world, id).into_iter().collect();
    for (guid, before) in old {
        if let Some(after) = new.get(&guid) {
            crate::command::record_respawn(world, before, *after);
        }
    }
    if let Some(mut selection) = world.get_resource_mut::<crate::selection::Selection>() {
        selection.clear();
    }
    Ok(())
}

/// A level version restored over the open level: one undo entry; undo puts the contents back.
pub struct RestoreLevelVersion {
    /// The open level's contents before the restore.
    pub before: regia_scene::prelude::Level,
    /// The version's contents (under the open level's id).
    pub after: regia_scene::prelude::Level,
    /// What the undo history shows.
    pub label: String,
}

impl crate::command::EditorCommand for RestoreLevelVersion {
    fn apply(&mut self, world: &mut World) {
        if let Err(err) = replace_level(world, &self.after) {
            tracing::warn!(target: "regia_editor", "restore version: {err}");
        }
    }

    fn revert(&mut self, world: &mut World) {
        if let Err(err) = replace_level(world, &self.before) {
            tracing::warn!(target: "regia_editor", "undo restore version: {err}");
        }
    }

    fn merge(&mut self, _next: &dyn crate::command::EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        self.label.clone()
    }
}

/// A document version opened in its editor: one undo entry; undo opens the bytes it replaced.
/// Both directions leave the document unsaved (its file changes only when the user saves).
pub struct RestoreDocVersion {
    /// The document.
    pub key: DocKey,
    /// Its editor's restore hook.
    pub restore: crate::doc_autosave::RestoreFn,
    /// The document's bytes before the restore.
    pub before: Vec<u8>,
    /// The version's bytes.
    pub after: Vec<u8>,
    /// What the undo history shows.
    pub label: String,
}

impl crate::command::EditorCommand for RestoreDocVersion {
    fn apply(&mut self, world: &mut World) {
        if let Err(err) = (self.restore)(world, &self.key, &self.after) {
            tracing::warn!(target: "regia_editor", "restore version: {err}");
        }
    }

    fn revert(&mut self, world: &mut World) {
        if let Err(err) = (self.restore)(world, &self.key, &self.before) {
            tracing::warn!(target: "regia_editor", "undo restore version: {err}");
        }
    }

    fn merge(&mut self, _next: &dyn crate::command::EditorCommand) -> bool {
        false
    }

    fn changes_level_state(&self) -> bool {
        false
    }

    fn description(&self) -> String {
        self.label.clone()
    }
}

/// What a restore did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOutcome {
    /// The version restored.
    pub version: Version,
    /// The undo entry's description.
    pub undo: String,
    /// The `before_restore` copy of a document's state, when one was written.
    pub kept: bool,
}

/// Restores version `id` as a new undoable change (never written over a file). A level version
/// needs its level open; a document version opens in its editor.
///
/// # Errors
///
/// See [`RestoreError::code`].
pub fn restore(world: &mut World, id: &str) -> Result<RestoreOutcome, RestoreError> {
    let root = crate::matgraph_editor::project_root(world)
        .ok_or_else(|| RestoreError::new("no_project", "no project is open"))?;
    let version =
        find(&root, id).ok_or_else(|| RestoreError::new("no_such_version", format!("no version `{id}`")))?;
    let bytes = std::fs::read(&version.file).map_err(|e| {
        RestoreError::new(
            "version_unreadable",
            format!("cannot read {}: {e}", version.file.display()),
        )
    })?;
    let when =
        crate::content::rfc3339_from_unix(i64::try_from(version.saved_unix_ms / 1000).unwrap_or(i64::MAX));
    let label = format!("Restore {} version of {}", when, version.target.label);
    if version.target.kind == LEVEL_KIND {
        restore_level(world, &root, version, &bytes, label)
    } else {
        restore_doc(world, &root, version, bytes, label)
    }
}

fn restore_level(
    world: &mut World,
    root: &Path,
    version: Version,
    bytes: &[u8],
    label: String,
) -> Result<RestoreOutcome, RestoreError> {
    if crate::prefab_edit::in_prefab_mode(world) {
        return Err(RestoreError::new(
            "prefab_open",
            "Close the prefab editor first (editor.close_prefab) to restore a level version",
        ));
    }
    let open = world
        .get_resource::<crate::level_io::OpenLevel>()
        .cloned()
        .filter(|open| VersionTarget::level(root, open.path.as_deref()).id() == version.target.id())
        .ok_or_else(|| {
            RestoreError::new(
                "version_level_not_open",
                format!("open {} first (editor.open_level)", version.target.label),
            )
        })?;
    let text = std::str::from_utf8(bytes)
        .map_err(|_| RestoreError::new("version_unreadable", "the level version is not text"))?;
    let after = regia_scene::prelude::Level::from_ron_str(open.id, text).map_err(|e| {
        RestoreError::new(
            "version_unreadable",
            format!("the level version does not parse: {e}"),
        )
    })?;
    crate::level_io::adopt_runtime_spawns(world, open.id);
    let mut before = crate::level_io::snapshot_saved_level(world, open.id).map_err(|e| {
        RestoreError::new(
            "version_unreadable",
            format!("cannot snapshot the open level: {e}"),
        )
    })?;
    crate::level_io::keep_layer_binding(world, open.id, &mut before.scene);
    let command = RestoreLevelVersion {
        before,
        after,
        label: label.clone(),
    };
    // The undo stack takes applied commands.
    replace_level(world, &command.after).map_err(|e| RestoreError::new("version_unreadable", e))?;
    let mut stack =
        world.get_resource_or_insert_with::<crate::command::UndoStack>(crate::command::UndoStack::new);
    stack.push(Box::new(command));
    stack.seal();
    tracing::info!(target: "regia_editor", "restored level version {}", version.id);
    Ok(RestoreOutcome {
        version,
        undo: label,
        kept: false,
    })
}

fn restore_doc(
    world: &mut World,
    root: &Path,
    version: Version,
    after: Vec<u8>,
    label: String,
) -> Result<RestoreOutcome, RestoreError> {
    crate::doc_autosave::install(world);
    let provider = world
        .resource::<crate::doc_autosave::DocAutosaveRegistry>()
        .provider(&version.target.kind)
        .ok_or_else(|| {
            RestoreError::new(
                "version_no_editor",
                format!("no editor restores `{}` documents", version.target.kind),
            )
        })?;
    let original = version.target.original_path(root);
    let key = DocKey::new(&version.target.kind, original.as_deref());
    let key = DocKey {
        label: version.target.label.clone(),
        ..key
    };
    // The document's current state: its unsaved edits when it has any, else its file.
    let dirty = (provider.dirty)(world).iter().any(|k| k.id() == key.id());
    let before = if dirty {
        (provider.snapshot)(world)
            .into_iter()
            .find(|s| s.key.id() == key.id())
            .map(|s| s.bytes)
    } else {
        original.as_deref().and_then(|p| std::fs::read(p).ok())
    }
    .unwrap_or_default();
    let mut kept = false;
    if dirty && !before.is_empty() {
        // The unsaved edits are only in memory: keep them as a version first.
        let write = VersionWrite {
            target: version.target.clone(),
            source: VersionSource::BeforeRestore,
            bytes: VersionBytes::Bytes(before.clone()),
        };
        kept = record(root, &write, &HistoryPolicy::of_world(world))
            .map_err(|e| RestoreError::new("version_unreadable", e))?
            .is_some();
    }
    (provider.restore)(world, &key, &after).map_err(|e| RestoreError::new("version_unreadable", e))?;
    let command = RestoreDocVersion {
        key,
        restore: provider.restore,
        before,
        after,
        label: label.clone(),
    };
    let mut stack =
        world.get_resource_or_insert_with::<crate::command::UndoStack>(crate::command::UndoStack::new);
    stack.push(Box::new(command));
    stack.seal();
    tracing::info!(target: "regia_editor", "restored document version {}", version.id);
    Ok(RestoreOutcome {
        version,
        undo: label,
        kept,
    })
}

// ---------------------------------------------------------------------------------------------
// The window.

/// The File ▸ Version History window's state.
#[derive(Resource, Debug, Default, Clone)]
pub struct VersionHistoryPanel {
    /// Whether the window is open.
    pub open: bool,
    /// The target the list shows ([`VersionTarget::id`]); `None`: the open level.
    pub target: Option<String>,
    /// The selected version.
    pub selected: Option<String>,
    /// The preview of the selected version, after Preview.
    pub preview: Option<VersionPreview>,
    /// The last result line.
    pub status: String,
    /// The frame (`cumulative_frame_nr`) a button was last applied in: egui may run the window's
    /// UI twice in one frame, and a restore must happen once.
    pub applied_frame: Option<u64>,
    /// The store as last read (review A4): kept while the store generation, the picked target and
    /// the open level are unchanged.
    list: Option<ListCache>,
}

/// What the window read from the store, and the inputs it was read for.
#[derive(Debug, Clone)]
struct ListCache {
    /// The project root.
    root: PathBuf,
    /// [`store_generation`] before the read.
    generation: u64,
    /// [`VersionHistoryPanel::target`] at the read.
    choice: Option<String>,
    /// The open level's path at the read (the default target).
    open_level: Option<PathBuf>,
    /// Every target with versions ([`targets`]).
    all: Vec<(String, VersionTarget)>,
    /// The target shown.
    target: VersionTarget,
    /// Its versions, newest first.
    versions: Vec<Version>,
}

impl VersionHistoryPanel {
    /// The versions the window lists (empty before its first frame or after [`Self::refresh`]).
    #[must_use]
    pub fn listed(&self) -> &[Version] {
        self.list.as_ref().map_or(&[], |list| list.versions.as_slice())
    }

    /// Drops the list read from the store, so the next frame reads it again.
    pub fn refresh(&mut self) {
        self.list = None;
    }

    /// Closes the window; opening it again reads the store fresh (a write by another process
    /// shows then).
    pub fn close(&mut self) {
        self.open = false;
        self.refresh();
    }
}

/// A button the window collected this frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelAction {
    /// Preview this version.
    Preview(String),
    /// Restore this version.
    Restore(String),
}

/// The target the window shows: the chosen one, else the open level's.
fn panel_target(
    root: &Path,
    choice: Option<&String>,
    open_level: Option<&Path>,
    all: &[(String, VersionTarget)],
) -> VersionTarget {
    choice
        .and_then(|id| all.iter().find(|(t, _)| t == id).map(|(_, t)| t.clone()))
        .unwrap_or_else(|| VersionTarget::level(root, open_level))
}

/// The window's list: the cached one while nothing it was read for changed, else a fresh read.
fn panel_list(world: &World, root: &Path, panel: &mut VersionHistoryPanel) -> ListCache {
    let open_level = world
        .get_resource::<crate::level_io::OpenLevel>()
        .and_then(|o| o.path.clone());
    // Read the generation before the store: a write landing during the read bumps it again, so
    // the next frame reads once more instead of keeping a list that misses it.
    let generation = store_generation(root);
    if let Some(list) = panel.list.take()
        && list.root == root
        && list.generation == generation
        && list.choice == panel.target
        && list.open_level == open_level
    {
        return list;
    }
    let _span = tracing::trace_span!("version_history_list").entered();
    let all = targets(root);
    let target = panel_target(root, panel.target.as_ref(), open_level.as_deref(), &all);
    let versions = versions_of(root, &target);
    ListCache {
        root: root.to_path_buf(),
        generation,
        choice: panel.target.clone(),
        open_level,
        all,
        target,
        versions,
    }
}

fn target_label(target: &VersionTarget) -> String {
    format!(
        "{} ({})",
        target.original.as_deref().unwrap_or(&target.label),
        target.kind
    )
}

/// Draws the window's contents: the target picker, the versions (time, source, size), Preview and
/// Restore, and the read-only preview. Every control is a focusable egui widget (Tab moves between
/// them; Space or Enter presses). Returns the button pressed this frame.
pub fn panel_ui(
    ui: &mut egui::Ui,
    world: &World,
    root: &Path,
    panel: &mut VersionHistoryPanel,
) -> Option<PanelAction> {
    let _span = tracing::trace_span!("version_history_panel").entered();
    let list = panel_list(world, root, panel);
    let (all, target, versions) = (&list.all, &list.target, &list.versions);
    let mut action = None;
    // A picker only when there is another target to pick.
    let only_this = all.iter().all(|(id, _)| *id == target.id());
    ui.horizontal(|ui| {
        ui.label("Versions of");
        if only_this {
            ui.strong(target_label(target));
            return;
        }
        egui::ComboBox::from_id_salt("version-history-target")
            .selected_text(target_label(target))
            .show_ui(ui, |ui| {
                for (id, candidate) in all {
                    if ui
                        .selectable_label(target.id() == *id, target_label(candidate))
                        .clicked()
                    {
                        panel.target = Some(id.clone());
                        panel.selected = None;
                        panel.preview = None;
                    }
                }
            })
            .response
            .on_hover_text("The level or document whose versions are listed");
    });
    if versions.is_empty() {
        ui.weak("No versions kept yet. Autosaves and saves keep one each.");
    }
    egui::ScrollArea::vertical()
        .id_salt("version-history-list")
        .max_height(220.0)
        .show(ui, |ui| {
            for version in versions {
                let when = crate::content::rfc3339_from_unix(
                    i64::try_from(version.saved_unix_ms / 1000).unwrap_or(i64::MAX),
                );
                let text = format!(
                    "{when}   {}   {}",
                    version.source.label(),
                    crate::doc_autosave::human_size(version.size)
                );
                let selected = panel.selected.as_deref() == Some(version.id.as_str());
                if ui.selectable_label(selected, text).clicked() {
                    panel.selected = Some(version.id.clone());
                    panel.preview = None;
                }
            }
        });
    let chosen = panel
        .selected
        .clone()
        .filter(|id| versions.iter().any(|v| &v.id == id));
    ui.horizontal(|ui| {
        let enabled = chosen.is_some();
        if crate::agent_parity::action_enabled(ui, enabled, &crate::agent_parity::VERSIONS_PREVIEW, None)
            .clicked()
            && let Some(id) = &chosen
        {
            action = Some(PanelAction::Preview(id.clone()));
        }
        if crate::agent_parity::action_enabled(ui, enabled, &crate::agent_parity::VERSIONS_RESTORE, None)
            .on_hover_text(
                "Restores the version as a new change you can undo; nothing is saved until you save",
            )
            .clicked()
            && let Some(id) = &chosen
        {
            action = Some(PanelAction::Restore(id.clone()));
        }
    });
    if !panel.status.is_empty() {
        ui.label(&panel.status);
    }
    if let Some(preview) = &panel.preview {
        ui.separator();
        ui.label(format!("Preview (read-only): {}", preview.version.id));
        if let Some(entities) = &preview.entities {
            ui.weak(format!("{} entities", entities.len()));
        }
        egui::ScrollArea::vertical()
            .id_salt("version-history-preview")
            .max_height(260.0)
            .show(ui, |ui| match &preview.text {
                Some(text) => {
                    // A `&str` buffer is read-only: the text can be selected and copied, not edited.
                    // Monospace without `code_editor()`, which would also keep Tab in the text
                    // (lock_focus) and trap keyboard focus there (review A5).
                    let mut view: &str = text.as_str();
                    ui.add(
                        egui::TextEdit::multiline(&mut view)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY),
                    );
                }
                None => {
                    ui.weak("Binary data (no text preview).");
                }
            });
        if preview.truncated {
            ui.weak("Preview cut at 64 KB.");
        }
    }
    panel.list = Some(list);
    action
}

/// Applies a window button: Preview reads the version, Restore restores it.
pub fn apply_panel_action(world: &mut World, root: &Path, action: &PanelAction) {
    let status = match action {
        PanelAction::Preview(id) => match find(root, id)
            .ok_or_else(|| format!("no version `{id}`"))
            .and_then(|v| preview(&v))
        {
            Ok(preview) => {
                world
                    .get_resource_or_insert_with(VersionHistoryPanel::default)
                    .preview = Some(preview);
                String::new()
            }
            Err(err) => err,
        },
        PanelAction::Restore(id) => match restore(world, id) {
            Ok(outcome) => format!("{} (Undo reverts it)", outcome.undo),
            Err(err) => err.message,
        },
    };
    let mut panel = world.get_resource_or_insert_with(VersionHistoryPanel::default);
    panel.status = status;
    // A restore may keep a `before_restore` version; read the list again either way.
    panel.refresh();
}

/// The window, when open (the shell calls it every frame).
pub fn window(ctx: &egui::Context, world: &mut World) {
    let Some(root) = crate::matgraph_editor::project_root(world) else {
        return;
    };
    if !world
        .get_resource::<VersionHistoryPanel>()
        .is_some_and(|panel| panel.open)
    {
        return;
    }
    // Taken out for the frame (not cloned: it holds the listed versions) and put back below.
    let Some(mut panel) = world.remove_resource::<VersionHistoryPanel>() else {
        return;
    };
    let mut open = true;
    let mut action = None;
    egui::Window::new("Version History")
        .open(&mut open)
        .default_width(560.0)
        .show(ctx, |ui| {
            action = panel_ui(ui, world, &root, &mut panel);
        });
    let frame = ctx.cumulative_frame_nr();
    let action = action.filter(|_| panel.applied_frame != Some(frame));
    if action.is_some() {
        panel.applied_frame = Some(frame);
    }
    if !open {
        panel.close();
    }
    world.insert_resource(panel);
    if let Some(action) = action {
        apply_panel_action(world, &root, &action);
    }
}

// ---------------------------------------------------------------------------------------------
// Agent surface.

fn no_project() -> RpcError {
    RpcError::new(
        UNSUPPORTED,
        "no_project",
        "no project is open",
        "open a project first (editor.open_project)",
    )
}

fn version_json(version: &Version) -> Value {
    json!({
        "id": version.id,
        "source": version.source.as_str(),
        "saved_unix_ms": version.saved_unix_ms,
        "size": version.size,
    })
}

fn target_json(id: &str, target: &VersionTarget) -> Value {
    json!({ "id": id, "kind": target.kind, "original": target.original, "label": target.label })
}

fn id_param(params: &Value) -> Result<String, RpcError> {
    params
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            RpcError::new(
                INVALID_PARAMS,
                "invalid_version_id",
                "`id` is required",
                "pass a version id from editor.versions",
            )
        })
}

/// The four methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.versions",
                tool: "versions",
                description: "The kept versions of a level or asset document (.regia/history/, newest first: autosaves, saves and before-restore copies, ADR-7928), and every target that has versions. Without `path`: the open level's.",
                params: &[ParamSchema {
                    name: "path",
                    kind: "string",
                    required: false,
                    description: "Project-relative path of the level or document (for example `assets/maps/main.level.ron`); omitted = the open level.",
                }],
                result: "{ target: { id, kind, original, label }, versions: [{ id, source, saved_unix_ms, size }], targets: [{ id, kind, original, label }], store_bytes }",
                mutating: false,
                permission: Permission::Read,
            },
            versions as Handler,
        ),
        (
            MethodSchema {
                name: "editor.version_preview",
                tool: "version_preview",
                description: "Reads one kept version read-only: its text (up to 64 KB) and, for a level, its entity names. Nothing in the editor changes.",
                params: &[ParamSchema {
                    name: "id",
                    kind: "string",
                    required: true,
                    description: "A version id from editor.versions.",
                }],
                result: "{ id, target: { id, kind, original, label }, source, saved_unix_ms, size, text: string | null, truncated, entities: [string] | null }",
                mutating: false,
                permission: Permission::Read,
            },
            version_preview as Handler,
        ),
        (
            MethodSchema {
                name: "editor.version_restore",
                tool: "version_restore",
                description: "Restores a kept version as a new undoable change: a level version replaces the open level's contents (the level must be open), a document version opens in its editor as unsaved edits (its unsaved state is kept as a before_restore version first). No file is written until a save.",
                params: &[ParamSchema {
                    name: "id",
                    kind: "string",
                    required: true,
                    description: "A version id from editor.versions.",
                }],
                result: "{ restored, target: { id, kind, original, label }, undo, kept }",
                mutating: true,
                permission: Permission::EditScene,
            },
            version_restore as Handler,
        ),
        (
            MethodSchema {
                name: "editor.duplicate_project",
                tool: "duplicate_project",
                description: "File ▸ Duplicate project…: copies the open project's source files (no target/, build/, dist/, .git/, .regia/ or cache folders) to a new folder. The open project stays open.",
                params: &[ParamSchema {
                    name: "dest",
                    kind: "string",
                    required: true,
                    description: "The new project folder: absolute, or relative to the folder that holds the open project. It must not exist (an empty folder is fine) and must be outside the project.",
                }],
                result: "{ root, files, bytes, skipped: [string] }",
                mutating: true,
                permission: Permission::EditScene,
            },
            duplicate_project as Handler,
        ),
    ]
}

/// Where `editor.duplicate_project` copies to: `dest` as given when absolute, else beside the
/// project.
#[must_use]
pub fn duplicate_dest(project_root: &Path, dest: &str) -> PathBuf {
    let path = Path::new(dest);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.parent().unwrap_or(project_root).join(path)
    }
}

fn duplicate_project(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = crate::matgraph_editor::project_root(world).ok_or_else(no_project)?;
    let dest = params
        .get("dest")
        .and_then(Value::as_str)
        .filter(|d| !d.trim().is_empty())
        .ok_or_else(|| {
            RpcError::new(
                INVALID_PARAMS,
                "invalid_dest",
                "`dest` is required",
                "pass the new project folder",
            )
        })?;
    let dest = duplicate_dest(&root, dest);
    let report = crate::project::duplicate_project(&root, &dest).map_err(|err| {
        let hint = match err.code {
            "duplicate_dest_exists" | "duplicate_dest_inside" => "choose a new folder outside the project",
            _ => "check the folder's permissions and free space",
        };
        RpcError::new(INVALID_PARAMS, err.code, err.message, hint)
    })?;
    Ok(json!({
        "root": report.root.display().to_string(),
        "files": report.files,
        "bytes": report.bytes,
        "skipped": report.skipped,
    }))
}

fn versions(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = crate::matgraph_editor::project_root(world).ok_or_else(no_project)?;
    let all = targets(&root);
    let target = match params.get("path") {
        None | Some(Value::Null) => {
            let open = world
                .get_resource::<crate::level_io::OpenLevel>()
                .and_then(|o| o.path.clone());
            VersionTarget::level(&root, open.as_deref())
        }
        Some(Value::String(path)) => {
            let wanted = path.replace('\\', "/");
            let wanted = wanted.trim_start_matches("./");
            all.iter()
                .map(|(_, t)| t)
                .find(|t| t.original.as_deref() == Some(wanted))
                .cloned()
                .unwrap_or_else(|| VersionTarget::level(&root, Some(&root.join(wanted))))
        }
        Some(_) => {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "invalid_path",
                "`path` must be a string",
                "pass a project-relative path, or omit it for the open level",
            ));
        }
    };
    let list: Vec<Value> = versions_of(&root, &target).iter().map(version_json).collect();
    Ok(json!({
        "target": target_json(&target.id(), &target),
        "versions": list,
        "targets": all.iter().map(|(id, t)| target_json(id, t)).collect::<Vec<_>>(),
        "store_bytes": store_size(&root),
    }))
}

fn version_preview(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = crate::matgraph_editor::project_root(world).ok_or_else(no_project)?;
    let id = id_param(params)?;
    let version = find(&root, &id).ok_or_else(|| {
        RpcError::new(
            NOT_FOUND,
            "no_such_version",
            format!("no version `{id}`"),
            "editor.versions lists them",
        )
    })?;
    let preview = preview(&version).map_err(|e| {
        RpcError::new(
            NOT_FOUND,
            "version_unreadable",
            e,
            "editor.versions lists the versions on disk",
        )
    })?;
    Ok(json!({
        "id": version.id,
        "target": target_json(&version.target.id(), &version.target),
        "source": version.source.as_str(),
        "saved_unix_ms": version.saved_unix_ms,
        "size": version.size,
        "text": preview.text,
        "truncated": preview.truncated,
        "entities": preview.entities,
    }))
}

fn version_restore(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = id_param(params)?;
    match restore(world, &id) {
        Ok(outcome) => Ok(json!({
            "restored": outcome.version.id,
            "target": target_json(&outcome.version.target.id(), &outcome.version.target),
            "undo": outcome.undo,
            "kept": outcome.kept,
        })),
        Err(err) => {
            let (code, hint) = match err.code {
                "no_project" => return Err(no_project()),
                "no_such_version" => (NOT_FOUND, "editor.versions lists them"),
                "version_level_not_open" => (
                    INVALID_PARAMS,
                    "open the level with editor.open_level, then restore",
                ),
                "prefab_open" => (INVALID_PARAMS, "close it with editor.close_prefab"),
                "version_no_editor" => (
                    UNSUPPORTED,
                    "editor.autosave_state lists the kinds an editor restores",
                ),
                _ => (INVALID_PARAMS, "editor.version_preview shows the version's text"),
            };
            Err(RpcError::new(code, err.code, err.message, hint))
        }
    }
}

#[cfg(test)]
mod tests;
