// SPDX-License-Identifier: MPL-2.0
//! Document autosave and recovery (`docs/specs/19-editor.md` § 7, ADR-9770): the asset editors'
//! unsaved documents beside the level autosave of [`crate::autosave`].
//!
//! **Registry.** Every asset editor that keeps a dirty flag registers a [`DocProvider`] in the
//! [`DocAutosaveRegistry`] resource: `dirty` lists its documents with unsaved edits (cheap, run
//! every frame while autosaves exist), `snapshot` returns their bytes exactly as the editor's save
//! would write them, and `restore` opens bytes in the editor's panel at the original path, dirty.
//!
//! **Files.** Snapshots go to `<project>/.regia/autosave/docs/<id>.autosave` with an editor-only
//! index `index.ron` ([`DocIndex`]); every file is written atomically (temp + rename). The folder
//! is editor state under `.regia/`, never a content format: the cook and the packager walk
//! `assets/` only.
//!
//! **Off-thread.** The tick takes the snapshots on the main thread (the documents live in the
//! world) and hands the encoding of the level and every write to one [`AutosaveWorker`] thread.
//! One worker, one queue: a removal queued after a write runs after it, so a manual save that
//! clears a document's autosave never races the tick that wrote it. At most one tick is queued
//! at a time: a due tick waits while the last one is still queued or running, then writes the
//! latest snapshot. A job that panics is reported as an error and the worker keeps running.
//!
//! **Recovery.** [`RecoveryOffers`] lists what a crashed session left behind: the level autosaves
//! and every indexed document. Restore opens each in its panel ([`restore_doc`]); Discard deletes
//! the file and its index row. The agent surface is `editor.autosave_state`, `editor.autosave_now`,
//! `editor.recovery_list`, `editor.recovery_restore` and `editor.recovery_discard`
//! (`book/agents/editor-autosave.md`).

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::time::{Duration, Instant, SystemTime};

use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::atomic_write::write_atomic;
use crate::autosave::{self, RecoveryCandidate};

/// The document autosave folder under the project's autosave folder.
pub const DOCS_DIR: &str = "docs";
/// The editor-only index of the document autosaves.
pub const INDEX_FILE: &str = "index.ron";
/// The index's version (editor state, not a content format: an unknown version is ignored).
pub const INDEX_VERSION: u32 = 1;
/// The extension of one document's autosave bytes.
const DOC_EXTENSION: &str = "autosave";

/// `<project>/.regia/autosave/docs/`.
#[must_use]
pub fn docs_dir(project_root: &Path) -> PathBuf {
    autosave::autosave_dir(project_root).join(DOCS_DIR)
}

/// Which document a snapshot belongs to: the editor kind, the file it saves to (`None` while
/// untitled) and a label that tells untitled documents apart.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DocKey {
    /// The provider kind (`matgraph`, `ui_design`, ...).
    pub kind: String,
    /// The document's own file, when it has one.
    pub original: Option<PathBuf>,
    /// What the recovery list shows (and what tells two untitled documents apart).
    pub label: String,
}

impl DocKey {
    /// A key for `kind` at `original` (the label is the file name, or `untitled <kind>`).
    #[must_use]
    pub fn new(kind: &str, original: Option<&Path>) -> Self {
        let label = original.and_then(Path::file_name).map_or_else(
            || format!("untitled {kind}"),
            |name| name.to_string_lossy().into_owned(),
        );
        Self {
            kind: kind.to_string(),
            original: original.map(Path::to_path_buf),
            label,
        }
    }

    /// The stable id of the document's autosave: FNV-1a 64 of the kind and the original path
    /// (or the label while untitled), so each tick overwrites the same file.
    #[must_use]
    pub fn id(&self) -> String {
        let mut text = self.kind.clone();
        text.push('\0');
        match &self.original {
            Some(path) => text.push_str(&path.to_string_lossy().replace('\\', "/")),
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
}

/// One document's unsaved state: the bytes its editor would save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocSnapshot {
    /// Which document.
    pub key: DocKey,
    /// The bytes, as the editor's save writes them.
    pub bytes: Vec<u8>,
}

/// Lists the documents of one editor that have unsaved edits.
pub type DirtyFn = fn(&World) -> Vec<DocKey>;
/// Takes the snapshots of one editor's unsaved documents.
pub type SnapshotFn = fn(&World) -> Vec<DocSnapshot>;
/// Opens `bytes` in the editor at the key's original path, marked dirty.
pub type RestoreFn = fn(&mut World, &DocKey, &[u8]) -> Result<(), String>;
/// Prepares one editor's snapshots with the world mutable, just before the tick takes them (an
/// editor whose snapshot needs `&mut World`, such as prefab edit mode's whole-scene snapshot,
/// keeps the bytes for its [`SnapshotFn`]). Runs only on a due tick.
pub type PrepareFn = fn(&mut World);

/// One asset editor's autosave hooks.
#[derive(Clone, Copy)]
pub struct DocProvider {
    /// The kind written into [`DocKey::kind`].
    pub kind: &'static str,
    /// The editor's dirty documents.
    pub dirty: DirtyFn,
    /// Their snapshots.
    pub snapshot: SnapshotFn,
    /// Reopens one snapshot.
    pub restore: RestoreFn,
}

impl std::fmt::Debug for DocProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocProvider")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// Saves one dirty document of an editor to its own file, as the editor's Save does, asking
/// nothing (the Save all dialog's and `editor.save_all`'s hook, P-147, ADR-8147). A document
/// that is no longer dirty saves as `Ok`.
pub type SaveFn = fn(&mut World, &DocKey) -> Result<(), String>;

/// One editor's save hook ([`DocAutosaveRegistry::register_save`]).
#[derive(Clone, Copy)]
pub struct DocSaveHook {
    /// The provider kind it saves.
    pub kind: &'static str,
    /// The save.
    pub save: SaveFn,
    /// Whether it also saves a document with no file of its own ([`DocKey::original`] `None`):
    /// an edit applied to project files, such as the Localization panel's edit box. Otherwise an
    /// untitled document is saved from its editor (it needs a file name).
    pub saves_untitled: bool,
}

impl std::fmt::Debug for DocSaveHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocSaveHook")
            .field("kind", &self.kind)
            .field("saves_untitled", &self.saves_untitled)
            .finish_non_exhaustive()
    }
}

/// One asset editor's documents it keeps open behind the one its panel shows (P-322 part b): a
/// table or vehicle opened before the current one stays in memory, with its unsaved edits, so a
/// shared undo can bring it back. Their keys carry the editor's own kind, so a recovery restores
/// them through that kind's [`DocProvider`]; a key the open documents already list is not taken
/// twice.
#[derive(Clone, Copy)]
pub struct RetainedProvider {
    /// The kind of the [`DocProvider`] that restores these documents.
    pub kind: &'static str,
    /// The retained documents with unsaved edits.
    pub dirty: DirtyFn,
    /// Their snapshots.
    pub snapshot: SnapshotFn,
}

impl std::fmt::Debug for RetainedProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetainedProvider")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// The asset editors that take part in document autosave.
#[derive(Resource, Debug, Default, Clone)]
pub struct DocAutosaveRegistry {
    providers: Vec<DocProvider>,
    prepares: Vec<(&'static str, PrepareFn)>,
    saves: Vec<DocSaveHook>,
    retained: Vec<RetainedProvider>,
}

impl DocAutosaveRegistry {
    /// Adds (or replaces, by kind) one editor's hooks.
    pub fn register(&mut self, provider: DocProvider) {
        self.providers.retain(|p| p.kind != provider.kind);
        self.providers.push(provider);
    }

    /// Adds (or replaces, by kind) one editor's preparation hook ([`PrepareFn`]).
    pub fn register_prepare(&mut self, kind: &'static str, prepare: PrepareFn) {
        self.prepares.retain(|(k, _)| *k != kind);
        self.prepares.push((kind, prepare));
    }

    /// Adds (or replaces, by kind) one editor's retained-document hooks ([`RetainedProvider`]).
    pub fn register_retained(&mut self, retained: RetainedProvider) {
        self.retained.retain(|p| p.kind != retained.kind);
        self.retained.push(retained);
    }

    /// The registered retained-document hooks.
    #[must_use]
    pub fn retained(&self) -> &[RetainedProvider] {
        &self.retained
    }

    /// The registered providers.
    #[must_use]
    pub fn providers(&self) -> &[DocProvider] {
        &self.providers
    }

    /// The provider of `kind`.
    #[must_use]
    pub fn provider(&self, kind: &str) -> Option<DocProvider> {
        self.providers.iter().find(|p| p.kind == kind).copied()
    }

    /// Every registered kind, in registration order.
    #[must_use]
    pub fn kinds(&self) -> Vec<&'static str> {
        self.providers.iter().map(|p| p.kind).collect()
    }

    /// Adds (or replaces, by kind) one editor's save hook ([`SaveFn`]).
    pub fn register_save(&mut self, kind: &'static str, save: SaveFn, saves_untitled: bool) {
        self.saves.retain(|hook| hook.kind != kind);
        self.saves.push(DocSaveHook {
            kind,
            save,
            saves_untitled,
        });
    }

    /// The save hook of `kind`.
    #[must_use]
    pub fn save_hook(&self, kind: &str) -> Option<DocSaveHook> {
        self.saves.iter().find(|hook| hook.kind == kind).copied()
    }
}

/// Saves one dirty document through its editor's save hook.
///
/// # Errors
///
/// No save hook is registered for the kind, or the editor's save failed.
pub fn save_doc(world: &mut World, key: &DocKey) -> Result<(), String> {
    let hook = world
        .get_resource::<DocAutosaveRegistry>()
        .and_then(|registry| registry.save_hook(&key.kind))
        .ok_or_else(|| format!("{} documents are saved from their editor", key.kind))?;
    (hook.save)(world, key)
}

/// The dirty documents of every registered editor: the open ones, then the retained ones
/// ([`RetainedProvider`]) the open ones do not already list.
#[must_use]
pub fn dirty_documents(world: &World) -> Vec<DocKey> {
    let Some(registry) = world.get_resource::<DocAutosaveRegistry>() else {
        return Vec::new();
    };
    let mut keys: Vec<DocKey> = registry.providers.iter().flat_map(|p| (p.dirty)(world)).collect();
    let mut seen: BTreeSet<String> = keys.iter().map(DocKey::id).collect();
    for key in registry.retained.iter().flat_map(|p| (p.dirty)(world)) {
        if seen.insert(key.id()) {
            keys.push(key);
        }
    }
    keys
}

/// Runs every registered preparation hook (main thread, before [`take_snapshots`]).
pub fn prepare_snapshots(world: &mut World) {
    let prepares = world
        .get_resource::<DocAutosaveRegistry>()
        .map(|registry| registry.prepares.clone())
        .unwrap_or_default();
    for (_, prepare) in prepares {
        prepare(world);
    }
}

/// The snapshots of every registered editor's dirty documents (main thread), the retained ones
/// ([`RetainedProvider`]) after the open ones, each key once.
#[must_use]
pub fn take_snapshots(world: &World) -> Vec<DocSnapshot> {
    let Some(registry) = world.get_resource::<DocAutosaveRegistry>() else {
        return Vec::new();
    };
    let mut snapshots: Vec<DocSnapshot> = registry
        .providers
        .iter()
        .flat_map(|p| (p.snapshot)(world))
        .collect();
    let mut seen: BTreeSet<String> = snapshots.iter().map(|s| s.key.id()).collect();
    for snapshot in registry.retained.iter().flat_map(|p| (p.snapshot)(world)) {
        if seen.insert(snapshot.key.id()) {
            snapshots.push(snapshot);
        }
    }
    snapshots
}

/// One row of the document autosave index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocIndexEntry {
    /// [`DocKey::id`].
    pub id: String,
    /// The provider kind.
    pub kind: String,
    /// The document's own file, `/`-separated, project-relative when under the project.
    pub original: Option<String>,
    /// The label the recovery list shows.
    pub label: String,
    /// The bytes file, relative to the docs folder.
    pub file: String,
    /// Size of the bytes file.
    pub size: u64,
    /// When the snapshot was written, Unix milliseconds.
    pub saved_unix_ms: u64,
}

/// The editor-only index of `.regia/autosave/docs/` (not a content format; never cooked).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocIndex {
    /// [`INDEX_VERSION`].
    pub version: u32,
    /// One row per autosaved document.
    pub docs: Vec<DocIndexEntry>,
}

impl Default for DocIndex {
    fn default() -> Self {
        Self {
            version: INDEX_VERSION,
            docs: Vec::new(),
        }
    }
}

/// Whether `id` has the shape [`DocKey::id`] writes: 16 lowercase hex digits. Every id that names
/// a file is checked with this first, so an edited index (`../x`, an absolute path, a drive
/// letter, a backslash, NUL) never reaches outside the docs folder.
#[must_use]
pub fn is_valid_doc_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Whether an index row's `file` is a plain file name inside the docs folder: no separator
/// (`/`, `\`), drive or stream colon, NUL, leading dot, and one normal path component.
fn is_plain_file_name(file: &str) -> bool {
    !file.is_empty()
        && !file.starts_with('.')
        && !file.contains(['/', '\\', ':', '\0'])
        && matches!(
            Path::new(file).components().collect::<Vec<_>>().as_slice(),
            [std::path::Component::Normal(_)]
        )
}

/// Parses index text: a newer version or text that is not an index is an error (the message says
/// which); rows whose `id` or `file` could name a path outside the docs folder are dropped, and the
/// second value counts them.
///
/// # Errors
///
/// `index version N is not 1` or `not an autosave index: <parse error>`.
pub fn parse_index(text: &str) -> Result<(DocIndex, usize), String> {
    match ron::from_str::<DocIndex>(text) {
        Ok(mut index) if index.version == INDEX_VERSION => {
            let before = index.docs.len();
            index
                .docs
                .retain(|entry| is_valid_doc_id(&entry.id) && is_plain_file_name(&entry.file));
            let dropped = before - index.docs.len();
            Ok((index, dropped))
        }
        Ok(index) => Err(format!("index version {} is not {INDEX_VERSION}", index.version)),
        Err(err) => Err(format!("not an autosave index: {err}")),
    }
}

/// The index's text as [`write_docs`] writes it.
///
/// # Errors
///
/// The index could not be encoded.
pub fn index_text(index: &DocIndex) -> Result<String, String> {
    ron::ser::to_string_pretty(index, regia_core::ron_pretty())
        .map_err(|e| format!("cannot encode the index: {e}"))
}

/// Reads the index (an absent, unreadable or newer index is empty: nothing is offered). Rows
/// whose `id` or `file` could name a path outside the docs folder are dropped.
#[must_use]
pub fn read_index(project_root: &Path) -> DocIndex {
    let path = docs_dir(project_root).join(INDEX_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return DocIndex::default();
    };
    match parse_index(&text) {
        Ok((index, dropped)) => {
            if dropped > 0 {
                tracing::warn!(
                    target: "regia_editor",
                    "ignoring {} row(s) of {} that name a path outside the docs folder",
                    dropped,
                    path.display()
                );
            }
            index
        }
        Err(err) => {
            tracing::warn!(target: "regia_editor", "ignoring {}: {err}", path.display());
            DocIndex::default()
        }
    }
}

fn write_index(project_root: &Path, index: &DocIndex) -> Result<(), String> {
    let dir = docs_dir(project_root);
    let path = dir.join(INDEX_FILE);
    if index.docs.is_empty() {
        let _ = std::fs::remove_file(&path);
        // Only an empty folder goes.
        let _ = std::fs::remove_dir(&dir);
        return Ok(());
    }
    let text = index_text(index).map_err(|e| format!("{}: {e}", path.display()))?;
    write_atomic(&path, text.as_bytes())
}

fn stored_original(project_root: &Path, original: Option<&Path>) -> Option<String> {
    original.map(|path| {
        path.strip_prefix(project_root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    })
}

/// The absolute original path of an index row.
#[must_use]
pub fn resolved_original(project_root: &Path, entry: &DocIndexEntry) -> Option<PathBuf> {
    entry.original.as_deref().map(|text| {
        let path = Path::new(text);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            project_root.join(path)
        }
    })
}

fn unix_ms(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Writes `snapshots` and updates the index (the worker's half of a tick; also callable on its
/// own). Returns the ids written.
///
/// # Errors
///
/// The first document or index write that failed; the documents written before it stay indexed.
pub fn write_docs(project_root: &Path, snapshots: &[DocSnapshot]) -> Result<Vec<String>, String> {
    let dir = docs_dir(project_root);
    let mut index = read_index(project_root);
    let mut written = Vec::new();
    let mut first_error = None;
    for snapshot in snapshots {
        let id = snapshot.key.id();
        let file = format!("{id}.{DOC_EXTENSION}");
        if let Err(err) = write_atomic(&dir.join(&file), &snapshot.bytes) {
            first_error.get_or_insert(err);
            continue;
        }
        index.docs.retain(|entry| entry.id != id);
        index.docs.push(DocIndexEntry {
            id: id.clone(),
            kind: snapshot.key.kind.clone(),
            original: stored_original(project_root, snapshot.key.original.as_deref()),
            label: snapshot.key.label.clone(),
            file,
            size: snapshot.bytes.len() as u64,
            saved_unix_ms: unix_ms(SystemTime::now()),
        });
        written.push(id);
    }
    index.docs.sort_by(|a, b| a.id.cmp(&b.id));
    if !written.is_empty() {
        write_index(project_root, &index)?;
    }
    match first_error {
        Some(err) => Err(err),
        None => Ok(written),
    }
}

/// Removes the autosaves of `ids` (their bytes and index rows). An id that is not
/// [`is_valid_doc_id`] names no file and is skipped.
///
/// # Errors
///
/// The index could not be rewritten.
pub fn remove_docs(project_root: &Path, ids: &[String]) -> Result<(), String> {
    if ids.is_empty() {
        return Ok(());
    }
    let dir = docs_dir(project_root);
    let mut index = read_index(project_root);
    for id in ids {
        if !is_valid_doc_id(id) {
            tracing::warn!(target: "regia_editor", "not removing document autosave {id:?}: not an autosave id");
            continue;
        }
        let _ = std::fs::remove_file(dir.join(format!("{id}.{DOC_EXTENSION}")));
    }
    let before = index.docs.len();
    index.docs.retain(|entry| !ids.contains(&entry.id));
    if index.docs.len() != before || index.docs.is_empty() {
        write_index(project_root, &index)?;
    }
    Ok(())
}

/// Removes every document autosave except `keep` (the clean exit: the documents still offered for
/// recovery stay).
///
/// # Errors
///
/// The index could not be rewritten.
pub fn clear_docs_except(project_root: &Path, keep: &[String]) -> Result<(), String> {
    let dir = docs_dir(project_root);
    if !dir.is_dir() {
        return Ok(());
    }
    let index = read_index(project_root);
    let gone: Vec<String> = index
        .docs
        .iter()
        .map(|entry| entry.id.clone())
        .filter(|id| !keep.contains(id))
        .collect();
    remove_docs(project_root, &gone)?;
    // Stray bytes files without an index row (a crash between a write and the index) go too.
    let indexed: Vec<String> = read_index(project_root).docs.into_iter().map(|e| e.id).collect();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for path in entries.filter_map(Result::ok).map(|e| e.path()) {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let is_doc = path.extension().and_then(|e| e.to_str()) == Some(DOC_EXTENSION);
            if is_doc && !keep.contains(&stem) && !indexed.contains(&stem) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    let _ = std::fs::remove_dir(&dir);
    Ok(())
}

/// One document a crashed session left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocRecovery {
    /// The index row.
    pub entry: DocIndexEntry,
    /// The absolute path of the bytes file.
    pub autosave: PathBuf,
    /// The absolute original path, when the document had one.
    pub original: Option<PathBuf>,
}

impl DocRecovery {
    /// The key the document is restored under.
    #[must_use]
    pub fn key(&self) -> DocKey {
        DocKey {
            kind: self.entry.kind.clone(),
            original: self.original.clone(),
            label: self.entry.label.clone(),
        }
    }
}

/// The documents indexed under `.regia/autosave/docs/` whose bytes are present.
#[must_use]
pub fn doc_recovery_candidates(project_root: &Path) -> Vec<DocRecovery> {
    let dir = docs_dir(project_root);
    let mut out: Vec<DocRecovery> = read_index(project_root)
        .docs
        .into_iter()
        .filter_map(|entry| {
            // Only a plain file name inside the docs folder (an edited index never reaches out;
            // `read_index` already dropped such rows, this keeps the reader honest on its own).
            if !is_valid_doc_id(&entry.id) || !is_plain_file_name(&entry.file) {
                return None;
            }
            let autosave = dir.join(&entry.file);
            autosave.is_file().then(|| DocRecovery {
                original: resolved_original(project_root, &entry),
                autosave,
                entry,
            })
        })
        .collect();
    out.sort_by_key(|d| std::cmp::Reverse(d.entry.saved_unix_ms));
    out
}

/// What the launch dialog offers: the level autosaves and the documents.
#[derive(Resource, Debug, Clone, Default)]
pub struct RecoveryOffers {
    /// The project the offers belong to.
    pub project_root: Option<PathBuf>,
    /// Level autosaves (newest first).
    pub levels: Vec<RecoveryCandidate>,
    /// Documents (newest first).
    pub docs: Vec<DocRecovery>,
    /// Bumped by every change (the shell adopts a change an agent made by comparing it).
    pub revision: u64,
}

/// One row of the recovery list (`editor.recovery_list`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryRow {
    /// `level:<file name>` or `doc:<id>`.
    pub id: String,
    /// `level` or the document kind.
    pub kind: String,
    /// What the row shows.
    pub label: String,
    /// The original file, when known.
    pub original: Option<PathBuf>,
    /// The autosave file.
    pub autosave: PathBuf,
    /// Size of the autosave file, bytes.
    pub size: u64,
    /// When it was written, Unix milliseconds.
    pub saved_unix_ms: u64,
}

impl RecoveryOffers {
    /// Everything left behind in `project_root`: level autosaves newer than their level, and
    /// every indexed document.
    #[must_use]
    pub fn scan(project_root: &Path) -> Self {
        Self {
            project_root: Some(project_root.to_path_buf()),
            levels: crate::project::recovery_for(project_root),
            docs: doc_recovery_candidates(project_root),
            revision: 0,
        }
    }

    /// Whether nothing is on offer.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.levels.is_empty() && self.docs.is_empty()
    }

    /// The rows the dialog and `editor.recovery_list` show: levels first, then documents.
    #[must_use]
    pub fn rows(&self) -> Vec<RecoveryRow> {
        let mut rows: Vec<RecoveryRow> = self
            .levels
            .iter()
            .map(|level| RecoveryRow {
                id: level_row_id(level),
                kind: "level".to_string(),
                label: level.original.as_deref().and_then(Path::file_name).map_or_else(
                    || "untitled level".to_string(),
                    |name| name.to_string_lossy().into_owned(),
                ),
                original: level.original.clone(),
                autosave: level.autosave.clone(),
                size: std::fs::metadata(&level.autosave).map_or(0, |m| m.len()),
                saved_unix_ms: unix_ms(level.modified),
            })
            .collect();
        rows.extend(self.docs.iter().map(|doc| RecoveryRow {
            id: format!("doc:{}", doc.entry.id),
            kind: doc.entry.kind.clone(),
            label: doc.entry.label.clone(),
            original: doc.original.clone(),
            autosave: doc.autosave.clone(),
            size: doc.entry.size,
            saved_unix_ms: doc.entry.saved_unix_ms,
        }));
        rows
    }

    /// The pending level autosave files (kept by a clean exit).
    #[must_use]
    pub fn pending_level_files(&self) -> Vec<PathBuf> {
        self.levels.iter().map(|l| l.autosave.clone()).collect()
    }

    /// The pending document ids (kept by a clean exit).
    #[must_use]
    pub fn pending_doc_ids(&self) -> Vec<String> {
        self.docs.iter().map(|d| d.entry.id.clone()).collect()
    }
}

/// The row id of a level autosave: `level:<autosave file name>`.
#[must_use]
pub fn level_row_id(level: &RecoveryCandidate) -> String {
    format!(
        "level:{}",
        level
            .autosave
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    )
}

/// What one restore or discard did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecoveryOutcome {
    /// Ids handled.
    pub done: Vec<String>,
    /// Ids that failed, with the reason.
    pub failed: Vec<(String, String)>,
}

/// Restores `ids` (every row when empty): a level opens as the open level (dirty), a document
/// opens in its panel at its original path (dirty). A restored row leaves the offer; its
/// autosave file stays until the document is saved (it is the only copy until then).
pub fn restore(world: &mut World, ids: &[String]) -> RecoveryOutcome {
    let offers = world
        .get_resource::<RecoveryOffers>()
        .cloned()
        .unwrap_or_default();
    let wanted: Vec<String> = if ids.is_empty() {
        offers.rows().into_iter().map(|row| row.id).collect()
    } else {
        ids.to_vec()
    };
    let mut outcome = RecoveryOutcome::default();
    let mut left = offers.clone();
    for id in wanted {
        if let Some(level) = offers.levels.iter().find(|l| level_row_id(l) == id) {
            match autosave::apply_restore(world, level) {
                Ok(_) => {
                    left.levels.retain(|l| l.autosave != level.autosave);
                    outcome.done.push(id);
                }
                Err(err) => outcome.failed.push((id, err)),
            }
        } else if let Some(doc) = offers.docs.iter().find(|d| format!("doc:{}", d.entry.id) == id) {
            match restore_doc(world, doc) {
                Ok(()) => {
                    left.docs.retain(|d| d.entry.id != doc.entry.id);
                    outcome.done.push(id);
                }
                Err(err) => outcome.failed.push((id, err)),
            }
        } else {
            outcome.failed.push((id, "no such recovery row".to_string()));
        }
    }
    left.revision = offers.revision + 1;
    world.insert_resource(left);
    outcome
}

/// Opens one document autosave in its editor (the registry's `restore` hook).
///
/// # Errors
///
/// No editor registered for the kind, the file is unreadable, or the editor refused the bytes.
pub fn restore_doc(world: &mut World, doc: &DocRecovery) -> Result<(), String> {
    let provider = world
        .get_resource::<DocAutosaveRegistry>()
        .and_then(|r| r.provider(&doc.entry.kind))
        .ok_or_else(|| format!("no editor restores `{}` documents", doc.entry.kind))?;
    let bytes =
        std::fs::read(&doc.autosave).map_err(|e| format!("cannot read {}: {e}", doc.autosave.display()))?;
    (provider.restore)(world, &doc.key(), &bytes)?;
    // This session owns the autosave now: a save of the document clears it.
    if let Some(mut state) = world.get_resource_mut::<AutosaveState>() {
        state.written.insert(doc.entry.id.clone());
    }
    tracing::info!(target: "regia_editor", "restored {} from {}", doc.entry.label, doc.autosave.display());
    Ok(())
}

/// Discards `ids` (every row when empty): deletes the autosave files and index rows. A file that
/// is already gone counts as discarded.
pub fn discard(world: &mut World, ids: &[String]) -> RecoveryOutcome {
    let offers = world
        .get_resource::<RecoveryOffers>()
        .cloned()
        .unwrap_or_default();
    let wanted: Vec<String> = if ids.is_empty() {
        offers.rows().into_iter().map(|row| row.id).collect()
    } else {
        ids.to_vec()
    };
    let mut outcome = RecoveryOutcome::default();
    let mut left = offers.clone();
    for id in wanted {
        if let Some(level) = offers.levels.iter().find(|l| level_row_id(l) == id) {
            match std::fs::remove_file(&level.autosave) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                    outcome.failed.push((
                        id,
                        format!("could not discard {}: {err}", level.autosave.display()),
                    ));
                }
                _ => {
                    left.levels.retain(|l| l.autosave != level.autosave);
                    outcome.done.push(id);
                }
            }
        } else if let Some(doc) = offers.docs.iter().find(|d| format!("doc:{}", d.entry.id) == id) {
            let root = offers.project_root.clone().unwrap_or_default();
            match remove_docs(&root, std::slice::from_ref(&doc.entry.id)) {
                Ok(()) => {
                    left.docs.retain(|d| d.entry.id != doc.entry.id);
                    outcome.done.push(id);
                }
                Err(err) => outcome.failed.push((id, err)),
            }
        } else {
            outcome.failed.push((id, "no such recovery row".to_string()));
        }
    }
    left.revision = offers.revision + 1;
    world.insert_resource(left);
    outcome
}

/// A button the recovery list collected this frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryListAction {
    /// Restore these rows.
    Restore(Vec<String>),
    /// Discard these rows.
    Discard(Vec<String>),
}

/// `1.2 KB`-style size.
#[must_use]
pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{:.1} MB", b / (KB * KB))
    }
}

/// The recovery list in the launch dialog: one row per level autosave or document (what, where,
/// when, size) with Restore and Discard, then Restore all and Discard all. Agent methods:
/// `editor.recovery_restore` and `editor.recovery_discard` (`ids` empty = all).
pub fn recovery_list_ui(ui: &mut egui::Ui, rows: &[RecoveryRow]) -> Option<RecoveryListAction> {
    let mut action = None;
    ui.label("A previous editor session left unsaved work behind:");
    egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
        egui::Grid::new("recovery-list").striped(true).show(ui, |ui| {
            for row in rows {
                ui.label(&row.label)
                    .on_hover_text(row.autosave.display().to_string());
                ui.weak(&row.kind);
                ui.weak(
                    row.original
                        .as_deref()
                        .map_or_else(|| "(untitled)".to_string(), |p| p.display().to_string()),
                );
                ui.weak(crate::content::rfc3339_from_unix(
                    i64::try_from(row.saved_unix_ms / 1000).unwrap_or(i64::MAX),
                ));
                ui.weak(human_size(row.size));
                if ui.button("Restore").clicked() {
                    action = Some(RecoveryListAction::Restore(vec![row.id.clone()]));
                }
                if ui.button("Discard").clicked() {
                    action = Some(RecoveryListAction::Discard(vec![row.id.clone()]));
                }
                ui.end_row();
            }
        });
    });
    ui.horizontal(|ui| {
        if ui.button("Restore all").clicked() {
            action = Some(RecoveryListAction::Restore(
                rows.iter().map(|r| r.id.clone()).collect(),
            ));
        }
        if ui.button("Discard all").clicked() {
            action = Some(RecoveryListAction::Discard(
                rows.iter().map(|r| r.id.clone()).collect(),
            ));
        }
    });
    action
}

// ---------------------------------------------------------------------------------------------
// The worker.

/// The level's half of a tick: where it goes and the encoding the worker runs.
pub struct LevelWrite {
    /// The autosave file.
    pub path: PathBuf,
    /// Encodes the level snapshot taken on the main thread (RON text).
    pub encode: Box<dyn FnOnce() -> Result<String, String> + Send>,
}

enum Job {
    Tick {
        root: PathBuf,
        level: Option<LevelWrite>,
        docs: Vec<DocSnapshot>,
        /// The level's version-history target (ADR-7928) and the retention policy.
        level_target: Option<crate::version_history::VersionTarget>,
        history: crate::version_history::HistoryPolicy,
    },
    /// Version-history copies (a manual save's file, ADR-7928).
    History {
        root: PathBuf,
        writes: Vec<crate::version_history::VersionWrite>,
        policy: crate::version_history::HistoryPolicy,
    },
    Remove {
        root: PathBuf,
        ids: Vec<String>,
    },
    ClearExcept {
        root: PathBuf,
        keep: Vec<String>,
    },
}

/// What one finished job reports back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobDone {
    /// The level autosave written, when the job had one.
    pub level: Option<PathBuf>,
    /// Document ids written.
    pub docs: Vec<String>,
    /// Errors (level or documents).
    pub errors: Vec<String>,
}

/// The autosave writer thread: jobs run one at a time in the order they were queued.
pub struct AutosaveWorker {
    jobs: Option<Sender<Job>>,
    /// In a mutex only so the resource is `Sync`; reached through `get_mut`, never locked.
    done: std::sync::Mutex<Receiver<JobDone>>,
    /// Jobs run on the calling thread because the worker is gone (reported by the next poll).
    inline: Vec<JobDone>,
    queued: u64,
    finished: u64,
    /// Whether each job queued and not reported back yet is a tick, oldest first (the worker
    /// reports in queue order).
    pending: VecDeque<bool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for AutosaveWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutosaveWorker")
            .field("queued", &self.queued)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl Default for AutosaveWorker {
    fn default() -> Self {
        Self::spawn()
    }
}

impl AutosaveWorker {
    /// Starts the writer thread.
    #[must_use]
    pub fn spawn() -> Self {
        let (jobs, rx) = channel::<Job>();
        let (tx, done) = channel::<JobDone>();
        let thread = std::thread::Builder::new()
            .name("regia-autosave".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    // A job that panics (an encoding runs here) is reported as an error; the
                    // thread lives on, so the jobs behind it still run and `flush` still drains.
                    let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_job(job)))
                        .unwrap_or_else(|payload| JobDone {
                            errors: vec![format!(
                                "autosave write panicked: {}",
                                panic_text(payload.as_ref())
                            )],
                            ..JobDone::default()
                        });
                    if tx.send(report).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| tracing::error!(target: "regia_editor", "autosave worker did not start: {e}"))
            .ok();
        Self {
            jobs: thread.as_ref().map(|_| jobs),
            done: std::sync::Mutex::new(done),
            inline: Vec::new(),
            queued: 0,
            finished: 0,
            pending: VecDeque::new(),
            thread,
        }
    }

    fn send(&mut self, job: Job) {
        let is_tick = matches!(job, Job::Tick { .. });
        let job = match &self.jobs {
            Some(jobs) => match jobs.send(job) {
                Ok(()) => {
                    self.queued += 1;
                    self.pending.push_back(is_tick);
                    return;
                }
                Err(std::sync::mpsc::SendError(job)) => job,
            },
            None => job,
        };
        // No thread (spawn failed or it ended): run here so nothing is lost.
        tracing::warn!(target: "regia_editor", "autosave worker unavailable; writing on the main thread");
        self.jobs = None;
        let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_job(job))).unwrap_or_else(
            |payload| JobDone {
                errors: vec![format!(
                    "autosave write panicked: {}",
                    panic_text(payload.as_ref())
                )],
                ..JobDone::default()
            },
        );
        self.inline.push(report);
    }

    /// Jobs queued but not yet reported back.
    #[must_use]
    pub fn in_flight(&self) -> u64 {
        self.queued - self.finished
    }

    /// Autosave ticks queued but not yet reported back (a due tick waits while this is not 0,
    /// so a slow worker never collects a backlog of snapshots).
    #[must_use]
    pub fn ticks_in_flight(&self) -> usize {
        self.pending.iter().filter(|&&tick| tick).count()
    }

    fn received(&mut self, done: JobDone, out: &mut Vec<JobDone>) {
        self.finished += 1;
        self.pending.pop_front();
        out.push(done);
    }

    /// The worker thread is gone with jobs still counted (it cannot report them): count them
    /// finished, report the loss, and run later jobs on the calling thread.
    fn lost_worker(&mut self, out: &mut Vec<JobDone>) {
        let lost = self.in_flight();
        if lost > 0 {
            out.push(JobDone {
                errors: vec![format!(
                    "autosave worker stopped; {lost} queued write(s) did not run"
                )],
                ..JobDone::default()
            });
        }
        self.finished = self.queued;
        self.pending.clear();
        self.jobs = None;
    }

    /// Collects finished jobs without waiting.
    pub fn poll(&mut self) -> Vec<JobDone> {
        let mut out = std::mem::take(&mut self.inline);
        loop {
            let rx = self
                .done
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match rx.try_recv() {
                Ok(done) => self.received(done, &mut out),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.lost_worker(&mut out);
                    break;
                }
            }
        }
        out
    }

    /// Waits until every queued job finished (tests, the exit path); `bound` caps the wait.
    pub fn flush(&mut self, bound: Duration) -> Vec<JobDone> {
        let mut out = self.poll();
        let deadline = Instant::now().checked_add(bound);
        while self.in_flight() > 0 {
            let left = deadline.map_or(Duration::MAX, |deadline| {
                deadline.saturating_duration_since(Instant::now())
            });
            if left.is_zero() {
                break;
            }
            let rx = self
                .done
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match rx.recv_timeout(left) {
                Ok(done) => self.received(done, &mut out),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    self.lost_worker(&mut out);
                    break;
                }
            }
        }
        out
    }
}

/// The message of a panic payload.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "a non-text panic".to_string()
    }
}

impl Drop for AutosaveWorker {
    fn drop(&mut self) {
        // Closing the queue ends the thread after the queued jobs; never block a drop on it.
        self.jobs = None;
        let _ = self.thread.take();
    }
}

/// Records version-history copies on the worker; a failure is reported, never fatal (ADR-7928).
fn record_versions(
    root: &Path,
    writes: &[crate::version_history::VersionWrite],
    policy: &crate::version_history::HistoryPolicy,
    done: &mut JobDone,
) {
    for write in writes {
        if let Err(err) = crate::version_history::record(root, write, policy) {
            done.errors.push(format!("version history: {err}"));
        }
    }
}

fn run_job(job: Job) -> JobDone {
    let _span = tracing::info_span!(target: "regia_editor", "autosave_write").entered();
    let mut done = JobDone::default();
    match job {
        Job::Tick {
            root,
            level,
            docs,
            level_target,
            history,
        } => {
            use crate::version_history::{VersionBytes, VersionSource, VersionWrite};
            let mut versions = Vec::new();
            if let Some(level) = level {
                match (level.encode)()
                    .and_then(|text| write_atomic(&level.path, text.as_bytes()).map(|()| text))
                {
                    Ok(text) => {
                        done.level = Some(level.path);
                        // The autosave also keeps a version (ADR-7928).
                        versions.extend(level_target.map(|target| VersionWrite {
                            target,
                            source: VersionSource::Autosave,
                            bytes: VersionBytes::Bytes(text.into_bytes()),
                        }));
                    }
                    Err(err) => done.errors.push(format!("level autosave: {err}")),
                }
            }
            if !docs.is_empty() {
                match write_docs(&root, &docs) {
                    Ok(ids) => done.docs = ids,
                    Err(err) => done.errors.push(format!("document autosave: {err}")),
                }
                versions.extend(
                    docs.into_iter()
                        .filter(|doc| done.docs.contains(&doc.key.id()))
                        .map(|doc| VersionWrite {
                            target: crate::version_history::VersionTarget::doc(&root, &doc.key),
                            source: VersionSource::Autosave,
                            bytes: VersionBytes::Bytes(doc.bytes),
                        }),
                );
            }
            record_versions(&root, &versions, &history, &mut done);
        }
        Job::History { root, writes, policy } => record_versions(&root, &writes, &policy, &mut done),
        Job::Remove { root, ids } => {
            if let Err(err) = remove_docs(&root, &ids) {
                done.errors.push(err);
            }
        }
        Job::ClearExcept { root, keep } => {
            if let Err(err) = clear_docs_except(&root, &keep) {
                done.errors.push(err);
            }
        }
    }
    done
}

// ---------------------------------------------------------------------------------------------
// The tick.

/// The autosave state of the session (`editor.autosave_state`).
#[derive(Resource, Debug)]
pub struct AutosaveState {
    /// The interval (`None`: autosave off by preference).
    pub interval: Option<Duration>,
    /// When the last tick ran (or the session became dirty).
    pub last: Option<Instant>,
    /// The writer thread.
    pub worker: AutosaveWorker,
    /// Document ids this session autosaved (and still owns).
    pub written: BTreeSet<String>,
    /// Ticks that queued a write.
    pub ticks: u64,
    /// Main-thread cost of the last tick, microseconds (snapshots and queueing).
    pub last_tick_us: u64,
    /// The last level autosave the worker finished.
    pub last_level: Option<PathBuf>,
    /// The last error the worker reported.
    pub last_error: Option<String>,
    /// Writes the worker finished.
    pub completed: u64,
}

impl Default for AutosaveState {
    fn default() -> Self {
        Self {
            interval: Some(autosave::AUTOSAVE_INTERVAL),
            last: None,
            worker: AutosaveWorker::spawn(),
            written: BTreeSet::new(),
            ticks: 0,
            last_tick_us: 0,
            last_level: None,
            last_error: None,
            completed: 0,
        }
    }
}

impl AutosaveState {
    /// Queues version-history copies on the worker, behind the writes already queued (ADR-7928).
    pub fn queue_history(
        &mut self,
        project_root: &Path,
        writes: Vec<crate::version_history::VersionWrite>,
        policy: crate::version_history::HistoryPolicy,
    ) {
        self.worker.send(Job::History {
            root: project_root.to_path_buf(),
            writes,
            policy,
        });
    }

    /// Folds finished jobs into the state.
    pub fn absorb(&mut self, finished: Vec<JobDone>) {
        for done in finished {
            self.completed += 1;
            if let Some(level) = done.level {
                self.last_level = Some(level);
            }
            for err in done.errors {
                tracing::warn!(target: "regia_editor", "{err}");
                self.last_error = Some(err);
            }
        }
    }
}

/// Installs the registry with the built-in editors, the state and the recovery offers.
pub fn install(world: &mut World) {
    if !world.contains_resource::<DocAutosaveRegistry>() {
        let mut registry = DocAutosaveRegistry::default();
        crate::doc_autosave_providers::register_builtin(&mut registry);
        world.insert_resource(registry);
    }
    if !world.contains_resource::<AutosaveState>() {
        world.insert_resource(AutosaveState::default());
    }
    if !world.contains_resource::<RecoveryOffers>() {
        world.insert_resource(RecoveryOffers::default());
    }
}

/// Inputs of one tick the caller decides (the shell or the headless loop).
#[derive(Debug, Clone, Copy)]
pub struct TickInput<'a> {
    /// The open project.
    pub project_root: &'a Path,
    /// Now (a parameter, so tests step time instead of sleeping).
    pub now: Instant,
    /// Write even when the interval has not passed (`editor.autosave_now`).
    pub force: bool,
}

/// What one tick did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Whether a write was queued.
    pub queued: bool,
    /// The level autosave path queued, when the level was dirty.
    pub level: Option<PathBuf>,
    /// Document ids queued.
    pub docs: Vec<String>,
    /// Main-thread microseconds.
    pub main_thread_us: u64,
}

/// Runs the autosave tick: collects finished writes, clears the autosaves of documents saved since
/// (every frame, cheap), and every interval snapshots the dirty level and documents on this thread
/// and queues their writes. A level autosave the recovery offer still holds is never overwritten.
pub fn tick(world: &mut World, input: TickInput<'_>) -> TickReport {
    let _span = tracing::info_span!(target: "regia_editor", "autosave_tick").entered();
    install(world);
    let started = Instant::now();
    let mut report = TickReport::default();
    {
        let mut state = world.resource_mut::<AutosaveState>();
        let finished = state.worker.poll();
        state.absorb(finished);
    }

    // A document this session autosaved and that is clean now was saved (or reverted): its
    // autosave goes, behind any write still queued for it.
    let owned = !world.resource::<AutosaveState>().written.is_empty();
    // A document that turns clean keeps a `save` version (ADR-7928): while saves keep versions the
    // dirty list is compared every frame the session owns an autosave (it is read then anyway), else
    // at most every `SAVE_CHECK_EVERY`.
    let history = crate::version_history::HistoryPolicy::of_world(world);
    let check_saves =
        history.keep_saves > 0 && (owned || crate::version_history::save_check_due(world, input.now));
    let dirty_docs = if owned || check_saves {
        dirty_documents(world)
    } else {
        Vec::new()
    };
    if check_saves {
        crate::version_history::note_doc_saves(world, input.project_root, &dirty_docs, input.now);
    }
    if owned {
        let dirty_ids: BTreeSet<String> = dirty_docs.iter().map(DocKey::id).collect();
        let pending: Vec<String> = world
            .get_resource::<RecoveryOffers>()
            .map(RecoveryOffers::pending_doc_ids)
            .unwrap_or_default();
        let mut state = world.resource_mut::<AutosaveState>();
        let saved: Vec<String> = state
            .written
            .iter()
            .filter(|id| !dirty_ids.contains(*id) && !pending.contains(id))
            .cloned()
            .collect();
        if !saved.is_empty() {
            for id in &saved {
                state.written.remove(id);
            }
            state.worker.send(Job::Remove {
                root: input.project_root.to_path_buf(),
                ids: saved,
            });
        }
    }

    // Due? (cheap; the dirty lists below are only gathered when it is.)
    let state = world.resource::<AutosaveState>();
    let last = state.last.unwrap_or(input.now);
    let due = input.force
        || state
            .interval
            .is_some_and(|every| input.now.saturating_duration_since(last) >= every);
    if state.last.is_none() {
        world.resource_mut::<AutosaveState>().last = Some(input.now);
    }
    if !due {
        return report;
    }
    // The last tick is still queued or running: this one waits (the timer is not reset, so the
    // first frame after the worker reports back writes the latest snapshot). Ticks are
    // idempotent, so nothing is lost, and a worker slower than the interval never collects a
    // backlog of snapshots. `editor.autosave_now` (forced) queues anyway and then flushes.
    if !input.force && world.resource::<AutosaveState>().worker.ticks_in_flight() > 0 {
        return report;
    }
    // In prefab edit mode the undo history is the prefab's and the open level is the mode's
    // scratch level: the level's state is the stash (dirty when it had unsaved edits on entry).
    let level_dirty = if crate::prefab_edit::in_prefab_mode(world) {
        stashed_level(world).is_some()
    } else {
        world
            .get_resource::<crate::command::UndoStack>()
            .is_some_and(crate::command::UndoStack::is_dirty)
    };
    let docs_dirty = if owned || check_saves {
        !dirty_docs.is_empty()
    } else {
        !dirty_documents(world).is_empty()
    };
    let any_dirty = level_dirty || docs_dirty;
    if !any_dirty {
        // A clean session never accumulates a stale timer.
        world.resource_mut::<AutosaveState>().last = Some(input.now);
        return report;
    }
    let (level, level_target) = if level_dirty {
        level_write(world, input.project_root)
            .map_or((None, None), |(write, target)| (Some(write), Some(target)))
    } else {
        (None, None)
    };
    prepare_snapshots(world);
    let docs = take_snapshots(world);
    report.level = level.as_ref().map(|l| l.path.clone());
    report.docs = docs.iter().map(|d| d.key.id()).collect();
    report.queued = level.is_some() || !docs.is_empty();
    let mut state = world.resource_mut::<AutosaveState>();
    state.last = Some(input.now);
    if report.queued {
        state.written.extend(report.docs.iter().cloned());
        state.worker.send(Job::Tick {
            root: input.project_root.to_path_buf(),
            level,
            docs,
            level_target,
            history,
        });
        state.ticks += 1;
    }
    report.main_thread_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    state.last_tick_us = report.main_thread_us;
    report
}

/// The level's write for a tick: the snapshot on this thread, the RON encoding on the worker, and
/// the level's version-history target (ADR-7928).
/// `None` when the open level's autosave is still on offer for recovery (writing over it would
/// destroy the crashed session's edits before the user chose) or the snapshot failed.
fn level_write(
    world: &mut World,
    project_root: &Path,
) -> Option<(LevelWrite, crate::version_history::VersionTarget)> {
    let stashed = stashed_level(world);
    let open = match &stashed {
        Some((open, _)) => open.clone(),
        None => world.get_resource::<crate::level_io::OpenLevel>().cloned()?,
    };
    let path = autosave::autosave_path(project_root, &open);
    let target = crate::version_history::VersionTarget::level(project_root, open.path.as_deref());
    let held = world
        .get_resource::<RecoveryOffers>()
        .is_some_and(|offers| offers.levels.iter().any(|c| c.autosave == path));
    if held {
        tracing::debug!(target: "regia_editor", "autosave skipped: a recovery candidate holds the same file");
        return None;
    }
    if let Some((_, level)) = stashed {
        return Some((
            LevelWrite {
                path,
                encode: Box::new(move || level.to_ron_string().map_err(|e| e.to_string())),
            },
            target,
        ));
    }
    // Runtime spawns are level content too (ADR-7382), with or without a shell frame before.
    crate::level_io::adopt_runtime_spawns(world, open.id);
    // The recovery copy is what a save writes: no `EditorOnly` entities (ADR-7205), no running
    // sequence preview's values (ADR-9710), the level's layer binding kept (ADR-7203). Only the
    // entities that changed since the last tick are reflected (ADR-7748).
    let snapshot = crate::autosave_incremental::snapshot_saved_level(world, open.id);
    match snapshot {
        Ok(mut level) => {
            crate::level_io::keep_layer_binding(world, open.id, &mut level.scene);
            Some((
                LevelWrite {
                    path,
                    encode: Box::new(move || level.to_ron_string().map_err(|e| e.to_string())),
                },
                target,
            ))
        }
        Err(err) => {
            tracing::warn!(target: "regia_editor", "autosave snapshot failed: {err}");
            None
        }
    }
}

/// The level prefab edit mode stashed, when it had unsaved edits as the mode was entered.
fn stashed_level(world: &World) -> Option<(crate::level_io::OpenLevel, regia_scene::prelude::Level)> {
    world
        .get_resource::<crate::prefab_edit::PrefabEditMode>()
        .filter(|mode| mode.editing().is_some())
        .and_then(crate::prefab_edit::PrefabEditMode::dirty_stashed_level)
}

/// The clean exit: every document autosave goes except the ones still on offer, after the
/// queued writes (waits up to `bound`).
pub fn exit_cleanup(world: &mut World, project_root: &Path, bound: Duration) {
    install(world);
    let keep = world
        .get_resource::<RecoveryOffers>()
        .map(RecoveryOffers::pending_doc_ids)
        .unwrap_or_default();
    let mut state = world.resource_mut::<AutosaveState>();
    state.worker.send(Job::ClearExcept {
        root: project_root.to_path_buf(),
        keep,
    });
    state.written.clear();
    let finished = state.worker.flush(bound);
    state.absorb(finished);
}

/// Waits for the queued writes (tests and `editor.autosave_now`); returns how many finished.
pub fn flush(world: &mut World, bound: Duration) -> usize {
    let Some(mut state) = world.get_resource_mut::<AutosaveState>() else {
        return 0;
    };
    let finished = state.worker.flush(bound);
    let n = finished.len();
    state.absorb(finished);
    n
}

#[cfg(test)]
mod tests;
