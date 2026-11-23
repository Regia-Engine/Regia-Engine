// SPDX-License-Identifier: MPL-2.0
//! Revision control in the editor (spec 19 addendum § 18.1-18.2, ADR-7837): git through its
//! command-line program, status badges on Content Browser rows and tiles, the Outliner's level row
//! and the status bar, and the Revision Control window (stage, unstage, discard, diff, commit with
//! validation, history, file history, branches).
//!
//! - [`git`]: the driver. Argument vectors only, never a shell string; `git` from `PATH` or the
//!   preference `vcs.git_command`; no credentials are handled (git's own configuration applies).
//! - [`RevisionControl`]: the provider trait. Git is the only provider in v0.6; the trait leaves room.
//! - [`VcsStatus`]: the status cache. [`tick_vcs_status_system`] runs `git status` on a worker thread
//!   only when a change notification moved its key (the project, the content index generation, the
//!   asset watcher's reload and error counts, the level's path and saved state, an explicit request),
//!   never per frame; `polls_started` / `polls_finished` count the runs.
//! - [`panel`]: the Revision Control window (View ▸ Revision Control, palette
//!   `view.toggle_revision_control`, the status bar's revision slot).
//! - [`agent`]: `editor.vcs_status`, `editor.vcs_stage`, `editor.vcs_unstage`, `editor.vcs_discard`,
//!   `editor.vcs_commit`, `editor.vcs_diff`, `editor.vcs_log`, `editor.vcs_branch`
//!   (`book/agents/revision-control.md`).
//!
//! - [`locks`]: git LFS file locks (lock on edit, lock badges, the save guard, unlock after commit;
//!   `editor.vcs_locks`, `editor.vcs_lock`, `editor.vcs_unlock`; ADR-7864).
//!
//! A project that is not in a git work tree shows "No revision control" and every badge is off.

pub mod agent;
pub mod git;
pub mod locks;
pub mod panel;
#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};

use regia_ecs::prelude::*;
use serde_json::{Value, json};

pub use git::{GitDriver, GitProgram};

/// The preference naming the git program (empty: `git` on `PATH`). Only the owner sets it.
pub const GIT_COMMAND_PREF: &str = "vcs.git_command";

/// What one side (staged or working tree) did to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// No change on this side.
    Unmodified,
    /// Contents changed.
    Modified,
    /// A new file.
    Added,
    /// The file is gone.
    Deleted,
    /// Moved from `orig_path`.
    Renamed,
    /// Copied from `orig_path`.
    Copied,
    /// File type changed (a file became a link, ...).
    TypeChanged,
}

impl Change {
    /// The change of a porcelain `XY` letter.
    #[must_use]
    pub fn from_code(code: char) -> Self {
        match code {
            'M' => Self::Modified,
            'A' => Self::Added,
            'D' => Self::Deleted,
            'R' => Self::Renamed,
            'C' => Self::Copied,
            'T' => Self::TypeChanged,
            _ => Self::Unmodified,
        }
    }

    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Unmodified => "unmodified",
            Self::Modified => "modified",
            Self::Added => "added",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::Copied => "copied",
            Self::TypeChanged => "type_changed",
        }
    }
}

/// How git sees a listed path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// Tracked, with a staged and/or a working-tree change.
    Tracked,
    /// Not tracked.
    Untracked,
    /// Ignored by an ignore rule (a directory entry ends with `/`).
    Ignored,
    /// An unmerged path (a merge conflict).
    Conflicted,
}

/// One path of the status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStatus {
    /// Project-relative, `/`-separated.
    pub path: String,
    /// The source of a rename or copy.
    pub orig_path: Option<String>,
    /// What the index holds against `HEAD`.
    pub staged: Change,
    /// What the working tree holds against the index.
    pub unstaged: Change,
    /// Tracked, untracked, ignored or conflicted.
    pub kind: EntryKind,
}

impl FileStatus {
    /// The one badge this path shows.
    #[must_use]
    pub fn state(&self) -> RevisionState {
        match self.kind {
            EntryKind::Conflicted => RevisionState::Conflicted,
            EntryKind::Untracked => RevisionState::Untracked,
            EntryKind::Ignored => RevisionState::Ignored,
            EntryKind::Tracked => {
                let either = |c: Change| self.staged == c || self.unstaged == c;
                if either(Change::Deleted) {
                    RevisionState::Deleted
                } else if self.staged == Change::Added {
                    RevisionState::Added
                } else if either(Change::Renamed) || either(Change::Copied) {
                    RevisionState::Renamed
                } else {
                    RevisionState::Modified
                }
            }
        }
    }

    /// Whether the index holds a change of this path (it shows in the Staged list).
    #[must_use]
    pub fn is_staged(&self) -> bool {
        self.kind == EntryKind::Tracked && self.staged != Change::Unmodified
    }

    /// Whether the working tree holds an unstaged change (it shows in the Changes list).
    #[must_use]
    pub fn is_unstaged(&self) -> bool {
        match self.kind {
            EntryKind::Tracked => self.unstaged != Change::Unmodified,
            EntryKind::Untracked | EntryKind::Conflicted => true,
            EntryKind::Ignored => false,
        }
    }

    /// The protocol form.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "orig_path": self.orig_path,
            "state": self.state().name(),
            "staged": self.staged.name(),
            "unstaged": self.unstaged.name(),
        })
    }
}

/// The revision badge of a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevisionState {
    /// Changed since the last commit.
    Modified,
    /// New and staged.
    Added,
    /// Deleted (staged or not).
    Deleted,
    /// Renamed or copied.
    Renamed,
    /// Not tracked.
    Untracked,
    /// A merge conflict.
    Conflicted,
    /// Ignored.
    Ignored,
}

impl RevisionState {
    /// The badge of one side's change (the Staged list shows the index side).
    #[must_use]
    pub fn from_change(change: Change) -> Self {
        match change {
            Change::Added => Self::Added,
            Change::Deleted => Self::Deleted,
            Change::Renamed | Change::Copied => Self::Renamed,
            _ => Self::Modified,
        }
    }

    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Modified => "modified",
            Self::Added => "added",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::Untracked => "untracked",
            Self::Conflicted => "conflicted",
            Self::Ignored => "ignored",
        }
    }

    /// The letter the badge shows (the state is never told by color alone).
    #[must_use]
    pub fn letter(self) -> &'static str {
        match self {
            Self::Modified => "M",
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Untracked => "?",
            Self::Conflicted => "!",
            Self::Ignored => "I",
        }
    }

    /// The hover text.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Modified => "Revision control: modified since the last commit",
            Self::Added => "Revision control: added (staged, not committed yet)",
            Self::Deleted => "Revision control: deleted",
            Self::Renamed => "Revision control: renamed",
            Self::Untracked => "Revision control: not tracked",
            Self::Conflicted => "Revision control: merge conflict",
            Self::Ignored => "Revision control: ignored",
        }
    }

    /// The badge color, from the theme's tokens.
    #[must_use]
    pub fn color(self, ctx: &egui::Context) -> egui::Color32 {
        let tokens = crate::theme::ui_tokens(ctx);
        match self {
            Self::Modified | Self::Renamed => tokens.accents.unsaved,
            Self::Added => tokens.accents.ok,
            Self::Deleted | Self::Conflicted => tokens.accents.error_text,
            Self::Untracked => tokens.accents.log_info,
            Self::Ignored => tokens.muted,
        }
    }
}

/// `git status` of the project.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoStatus {
    /// The current branch (`None` when `HEAD` is detached).
    pub branch: Option<String>,
    /// `HEAD` is detached.
    pub detached: bool,
    /// The commit `HEAD` names (`None` before the first commit).
    pub head: Option<String>,
    /// Every changed, untracked, ignored or conflicted path, sorted.
    pub files: Vec<FileStatus>,
}

impl RepoStatus {
    /// The row of `path` (exact).
    #[must_use]
    pub fn file(&self, path: &str) -> Option<&FileStatus> {
        self.files
            .binary_search_by(|f| f.path.as_str().cmp(path))
            .ok()
            .and_then(|i| self.files.get(i))
    }

    /// The badge of a project path: its own row, else an ignored or untracked directory above it.
    #[must_use]
    pub fn state_of(&self, path: &str) -> Option<RevisionState> {
        if let Some(file) = self.file(path) {
            return Some(file.state());
        }
        let mut end = 0;
        while let Some(slash) = path[end..].find('/') {
            end += slash + 1;
            if let Some(dir) = self.file(&path[..end])
                && matches!(dir.kind, EntryKind::Ignored | EntryKind::Untracked)
            {
                return Some(dir.state());
            }
        }
        None
    }

    /// The Staged list.
    pub fn staged(&self) -> impl Iterator<Item = &FileStatus> {
        self.files.iter().filter(|f| f.is_staged())
    }

    /// The Changes list (unstaged, untracked and conflicted).
    pub fn unstaged(&self) -> impl Iterator<Item = &FileStatus> {
        self.files.iter().filter(|f| f.is_unstaged())
    }

    /// Paths that are neither ignored nor clean: what the status bar counts.
    #[must_use]
    pub fn changed_count(&self) -> usize {
        self.files.iter().filter(|f| f.kind != EntryKind::Ignored).count()
    }

    /// Whether a tracked file has a staged or unstaged change, or a conflict (the branch-switch
    /// guard; untracked files travel with a switch and do not count).
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.files.iter().any(|f| {
            f.kind == EntryKind::Conflicted
                || (f.kind == EntryKind::Tracked
                    && (f.staged != Change::Unmodified || f.unstaged != Change::Unmodified))
        })
    }
}

/// One history row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// The full commit id.
    pub id: String,
    /// Author name.
    pub author: String,
    /// Author email.
    pub email: String,
    /// Author date, ISO 8601.
    pub date: String,
    /// The message's first line.
    pub summary: String,
}

impl LogEntry {
    /// The protocol form.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "author": self.author,
            "email": self.email,
            "date": self.date,
            "summary": self.summary,
        })
    }
}

/// The local branches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Branches {
    /// The current branch (`None`: detached `HEAD`).
    pub current: Option<String>,
    /// Every local branch, sorted.
    pub list: Vec<String>,
}

/// The diff of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// Project path.
    pub path: String,
    /// Index against `HEAD` (`true`) or working tree against the index.
    pub staged: bool,
    /// `text`, `binary` or `unchanged`.
    pub kind: &'static str,
    /// Unified diff text (`text` only).
    pub text: String,
    /// Bytes before (`binary`).
    pub old_size: Option<u64>,
    /// Bytes after (`binary`).
    pub new_size: Option<u64>,
    /// The text was cut at [`git::DIFF_TEXT_MAX`] bytes.
    pub truncated: bool,
}

impl FileDiff {
    fn set_text(&mut self, mut text: String) {
        if text.len() > git::DIFF_TEXT_MAX {
            let mut cut = git::DIFF_TEXT_MAX;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            self.truncated = true;
        }
        self.text = text;
    }

    /// The protocol form.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "staged": self.staged,
            "kind": self.kind,
            "text": self.text,
            "old_size": self.old_size,
            "new_size": self.new_size,
            "truncated": self.truncated,
        })
    }
}

/// A revision-control failure: a protocol code (lowercase ascii, digits, `_`) and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcsError {
    /// Stable code (listed in `book/agents/revision-control.md`).
    pub code: &'static str,
    /// What happened.
    pub message: String,
}

impl VcsError {
    /// An error with `code`.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for VcsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// A revision-control system the editor drives. Git ([`GitDriver`]) is the only one in v0.6.
pub trait RevisionControl: Send + Sync {
    /// The system's name (`git`).
    fn system(&self) -> &'static str;
    /// Every changed, untracked, ignored and conflicted path of the project.
    ///
    /// # Errors
    /// When the system fails.
    fn status(&self) -> Result<RepoStatus, VcsError>;
    /// Stages the paths (new, changed and deleted files alike).
    ///
    /// # Errors
    /// `invalid_path` or a system failure.
    fn stage(&self, paths: &[String]) -> Result<(), VcsError>;
    /// Takes the paths out of the index (their working-tree changes stay).
    ///
    /// # Errors
    /// `invalid_path` or a system failure.
    fn unstage(&self, paths: &[String]) -> Result<(), VcsError>;
    /// Drops the unstaged changes of the paths (tracked files return to their staged or committed
    /// contents; untracked files are deleted). Answers the paths it changed.
    ///
    /// # Errors
    /// `invalid_path`, `conflicts` or a system failure.
    fn discard(&self, paths: &[String]) -> Result<Vec<String>, VcsError>;
    /// Commits the staged changes; answers the new commit id.
    ///
    /// # Errors
    /// `empty_message`, `nothing_staged`, `conflicts` or a system failure.
    fn commit(&self, message: &str) -> Result<String, VcsError>;
    /// History, newest first: of the project, or of one path (following renames).
    ///
    /// # Errors
    /// `invalid_path` or a system failure.
    fn log(&self, path: Option<&str>, limit: usize) -> Result<Vec<LogEntry>, VcsError>;
    /// The local branches.
    ///
    /// # Errors
    /// A system failure.
    fn branches(&self) -> Result<Branches, VcsError>;
    /// Switches to an existing branch; refused while tracked files have changes.
    ///
    /// # Errors
    /// `invalid_branch`, `unknown_branch`, `dirty_tree` or a system failure.
    fn switch_branch(&self, name: &str) -> Result<(), VcsError>;
    /// Creates a branch at `HEAD`, switching to it when `switch`.
    ///
    /// # Errors
    /// `invalid_branch`, `branch_exists` or a system failure.
    fn create_branch(&self, name: &str, switch: bool) -> Result<(), VcsError>;
    /// The diff of one path.
    ///
    /// # Errors
    /// `invalid_path` or a system failure.
    fn diff(&self, path: &str, staged: bool) -> Result<FileDiff, VcsError>;
}

/// What the badges and the status bar show.
#[derive(Debug, Clone, PartialEq)]
pub enum VcsState {
    /// Not polled yet.
    Unknown,
    /// No project is open.
    NoProject,
    /// The project is not in a git work tree.
    NoRevisionControl,
    /// Git is missing or failed.
    Unavailable(VcsError),
    /// The last status.
    Ready(Arc<RepoStatus>),
}

impl VcsState {
    /// The status, when there is one.
    #[must_use]
    pub fn status(&self) -> Option<&Arc<RepoStatus>> {
        match self {
            Self::Ready(status) => Some(status),
            _ => None,
        }
    }

    /// The protocol name of the state.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::NoProject => "no_project",
            Self::NoRevisionControl => "no_revision_control",
            Self::Unavailable(_) => "unavailable",
            Self::Ready(_) => "ready",
        }
    }

    fn from_result(result: Result<RepoStatus, VcsError>) -> Self {
        match result {
            Ok(status) => Self::Ready(Arc::new(status)),
            Err(error) if error.code == "no_revision_control" => Self::NoRevisionControl,
            Err(error) if error.code == "no_project" => Self::NoProject,
            Err(error) => Self::Unavailable(error),
        }
    }
}

/// What makes the cache poll again: a change of any part.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChangeKey {
    root: Option<PathBuf>,
    program: GitProgram,
    content_generation: u64,
    watch: (u64, u64),
    level: Option<PathBuf>,
    level_dirty: bool,
    requests: u64,
}

/// The status cache (an editor-world resource).
#[derive(Resource, Debug)]
pub struct VcsStatus {
    /// What the hooks show.
    pub state: VcsState,
    /// Background `git status` runs started.
    pub polls_started: u64,
    /// Background runs finished.
    pub polls_finished: u64,
    /// Synchronous refreshes (agent calls and panel operations).
    pub refreshes: u64,
    /// Bumps whenever `state` is replaced.
    pub generation: u64,
    requests: u64,
    key: Option<ChangeKey>,
    inflight: Option<Mutex<Receiver<VcsState>>>,
    /// The `generation` the hooks were last filled for ([`publish`]).
    published: Option<u64>,
}

impl Default for VcsStatus {
    fn default() -> Self {
        Self {
            state: VcsState::Unknown,
            polls_started: 0,
            polls_finished: 0,
            refreshes: 0,
            generation: 0,
            requests: 0,
            key: None,
            inflight: None,
            published: None,
        }
    }
}

impl VcsStatus {
    /// Whether a background run is in flight.
    #[must_use]
    pub fn polling(&self) -> bool {
        self.inflight.is_some()
    }

    fn set_state(&mut self, state: VcsState) {
        self.state = state;
        self.generation += 1;
    }
}

/// The project root the editor runs in.
#[must_use]
pub fn project_root(world: &World) -> Option<PathBuf> {
    crate::mesh_source::project_root(world)
}

/// The git program: the `vcs.git_command` preference, else `git` on `PATH`.
#[must_use]
pub fn git_program(world: &World) -> GitProgram {
    world
        .get_resource::<crate::editor_prefs::EditorPreferences>()
        .map(|prefs| GitProgram::from_pref(&prefs.text(GIT_COMMAND_PREF)))
        .unwrap_or_default()
}

/// The driver of the open project.
///
/// # Errors
/// `no_project`, `no_revision_control`, `git_not_found` or `git_failed`.
pub fn driver(world: &World) -> Result<GitDriver, VcsError> {
    let root = project_root(world).ok_or_else(|| VcsError::new("no_project", "no project is open"))?;
    GitDriver::open(&root, git_program(world))
}

fn change_key(world: &World) -> ChangeKey {
    let watch = world
        .get_resource::<regia_asset::AssetWatchStatus>()
        .map_or((0, 0), |w| (w.reloads, w.errors));
    let level = world
        .get_resource::<crate::level_io::OpenLevel>()
        .and_then(|open| open.path.clone());
    let level_dirty = world
        .get_resource::<crate::command::UndoStack>()
        .is_some_and(crate::command::UndoStack::is_dirty);
    ChangeKey {
        root: project_root(world),
        program: git_program(world),
        content_generation: world
            .get_resource::<crate::content_index::ContentIndex>()
            .map_or(0, |index| index.generation),
        watch,
        level,
        level_dirty,
        requests: world.get_resource::<VcsStatus>().map_or(0, |s| s.requests),
    }
}

/// Asks for a background poll on the next tick (the panel's Refresh, the panel opening).
pub fn request_refresh(world: &mut World) {
    world.get_resource_or_insert_with(VcsStatus::default).requests += 1;
}

/// One tick: take a finished run's answer, and start a run when the change key moved. Nothing
/// runs while the key stays put (no per-frame process).
pub fn tick(world: &mut World) {
    tick_with(world, false);
}

/// [`tick`], then waits for the run it started or found in flight (tests and agents; the
/// worker's receive is the wait).
pub fn tick_and_wait(world: &mut World) {
    tick_with(world, true);
}

/// The `Last` system of the editor: [`tick`] once per frame.
pub fn tick_vcs_status_system(world: &mut World) {
    tick(world);
    // File locks: a background read when the project changes, the lock-on-edit watcher (ADR-7864).
    locks::tick(world);
}

fn tick_with(world: &mut World, block: bool) {
    let key = change_key(world);
    let mut status = world.remove_resource::<VcsStatus>().unwrap_or_default();
    receive(&mut status, false);
    if status.inflight.is_none() && status.key.as_ref() != Some(&key) {
        match key.root.clone() {
            None => {
                if status.state != VcsState::NoProject {
                    status.set_state(VcsState::NoProject);
                }
            }
            Some(root) => {
                let program = key.program.clone();
                let (tx, rx) = channel();
                let spawned = std::thread::Builder::new()
                    .name("vcs-status".into())
                    .spawn(move || {
                        let result = GitDriver::open(&root, program).and_then(|d| d.status());
                        let _ = tx.send(VcsState::from_result(result));
                    });
                match spawned {
                    Ok(_) => {
                        status.inflight = Some(Mutex::new(rx));
                        status.polls_started += 1;
                    }
                    Err(e) => status.set_state(VcsState::Unavailable(VcsError::new(
                        "git_failed",
                        format!("could not start the status worker: {e}"),
                    ))),
                }
            }
        }
        status.key = Some(key);
    }
    if block {
        receive(&mut status, true);
    }
    world.insert_resource(status);
    publish(world);
}

fn receive(status: &mut VcsStatus, block: bool) {
    let Some(rx) = status.inflight.take() else {
        return;
    };
    let answer = {
        let Ok(rx_guard) = rx.lock() else {
            return;
        };
        if block {
            rx_guard.recv().map_err(|_| TryRecvError::Disconnected)
        } else {
            rx_guard.try_recv()
        }
    };
    match answer {
        Ok(state) => {
            status.polls_finished += 1;
            status.set_state(state);
        }
        Err(TryRecvError::Empty) => status.inflight = Some(rx),
        Err(TryRecvError::Disconnected) => {
            status.polls_finished += 1;
            status.set_state(VcsState::Unavailable(VcsError::new(
                "git_failed",
                "the status worker stopped without an answer",
            )));
        }
    }
}

/// Runs `git status` now on this thread and stores it (agent calls and panel operations, so the
/// badges follow at once).
pub fn refresh_now(world: &mut World) -> VcsState {
    let state = match driver(world) {
        Ok(d) => VcsState::from_result(d.status()),
        Err(e) => VcsState::from_result(Err(e)),
    };
    let key = change_key(world);
    let mut status = world.get_resource_or_insert_with(VcsStatus::default);
    status.refreshes += 1;
    status.set_state(state.clone());
    // A run already in flight answers an older tree; the key stays so it is not repeated.
    status.key = Some(key);
    publish(world);
    state
}

/// The cached status, when the project is a git work tree.
#[must_use]
pub fn cached(world: &World) -> Option<Arc<RepoStatus>> {
    world
        .get_resource::<VcsStatus>()
        .and_then(|s| s.state.status().cloned())
}

/// The badge of a project path from the cache (`None`: clean, or no revision control).
#[must_use]
pub fn state_of(world: &World, path: &str) -> Option<RevisionState> {
    world
        .get_resource::<VcsStatus>()
        .and_then(|s| s.state.status().and_then(|status| status.state_of(path)))
}

/// The project path of `path` when it lies in the project.
#[must_use]
pub fn project_rel(world: &World, path: &std::path::Path) -> Option<String> {
    let root = project_root(world)?;
    let rel = crate::path_norm::strip_prefix_fold(&root, path, crate::path_norm::FOLD_CASE)?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

/// The project path of the open level file, when it lies in the project.
#[must_use]
pub fn level_path(world: &World) -> Option<String> {
    let path = world.get_resource::<crate::level_io::OpenLevel>()?.path.clone()?;
    project_rel(world, &path)
}

/// The open level's badge (a level directory takes the badge of any changed file inside it).
#[must_use]
pub fn level_state(world: &World) -> Option<RevisionState> {
    let rel = level_path(world)?;
    let status = cached(world)?;
    if let Some(state) = status.state_of(&rel) {
        return Some(state);
    }
    let dir = format!("{}/", rel.trim_end_matches('/'));
    status
        .files
        .iter()
        .find(|f| f.path.starts_with(&dir) && f.kind != EntryKind::Ignored)
        .map(|_| RevisionState::Modified)
}

// ---------------------------------------------------------------------------
// The producer: one fill of the revision hooks per poll
// ---------------------------------------------------------------------------

/// The status bar's label and its hover text for `state` (`None`: nothing to show yet). The branch
/// text carries no glyph: the drawn slot adds it ([`status_bar_slot`]).
fn slot_texts(state: &VcsState) -> Option<(String, String)> {
    match state {
        VcsState::Unknown | VcsState::NoProject => None,
        VcsState::NoRevisionControl => Some((
            "No revision control".to_string(),
            "The project is not in a git repository".to_string(),
        )),
        VcsState::Unavailable(error) => Some((format!("git: {}", error.code), error.message.clone())),
        VcsState::Ready(status) => {
            let branch = status.branch.clone().unwrap_or_else(|| "detached".to_string());
            let (label, tooltip) = match status.changed_count() {
                0 => (
                    format!("{branch} \u{b7} clean"),
                    format!("Revision control: branch {branch}, no changes"),
                ),
                n => (
                    format!("{branch} \u{b7} {n} changed"),
                    format!("Revision control: branch {branch}, {n} changed path(s)"),
                ),
            };
            Some((label, tooltip))
        }
    }
}

/// Fills the revision hooks from the status cache, once per answer ([`VcsStatus::generation`]):
/// [`crate::shell_status::RevisionSlot`] (what `editor.status_bar` reports),
/// [`crate::content_actions::AssetRevisionStates`] (the asset tooltip) and
/// [`crate::outliner_columns::RevisionColumn`] (the Outliner's Revision cells). It reads the
/// cached status only: no git call, and nothing when the answer did not change.
fn publish(world: &mut World) {
    let Some(status) = world.get_resource::<VcsStatus>() else {
        return;
    };
    if status.published == Some(status.generation) {
        return;
    }
    let generation = status.generation;
    let state = status.state.clone();
    if let Some(mut status) = world.get_resource_mut::<VcsStatus>() {
        status.published = Some(generation);
    }
    let _span = tracing::info_span!(target: "regia_editor", "vcs_publish").entered();
    let slot = slot_texts(&state)
        .map_or_else(crate::shell_status::RevisionSlot::default, |(label, tooltip)| {
            crate::shell_status::RevisionSlot { label, tooltip }
        });
    world.insert_resource(slot);
    let mut states = std::collections::BTreeMap::new();
    if let VcsState::Ready(status) = &state {
        for file in &status.files {
            states.insert(file.path.clone(), file.state().name().to_string());
        }
        // A level directory takes the state of any changed file inside it: the column reads the
        // level's own path.
        if let (Some(rel), Some(level)) = (level_path(world), level_state(world)) {
            states.entry(rel).or_insert_with(|| level.name().to_string());
        }
    }
    world.insert_resource(crate::content_actions::AssetRevisionStates(states));
    world.insert_resource(crate::outliner_columns::RevisionColumn(Some(revision_cell)));
}

/// The Outliner's Revision cell of `entity`: the state of the file the entity comes from, the open
/// level, or the prefab being edited (an entity of the level drawn around an Edit in place session
/// still reads the level). Empty for a clean file and without revision control.
#[must_use]
pub fn revision_cell(world: &World, entity: Entity) -> Option<String> {
    let prefab = world
        .get_resource::<crate::prefab_edit::PrefabEditMode>()
        .and_then(|mode| mode.path.as_deref())
        .filter(|_| world.get::<crate::prefab_edit::PrefabContext>(entity).is_none());
    let rel = match prefab {
        Some(path) => project_rel(world, path)?,
        None => level_path(world)?,
    };
    world
        .get_resource::<crate::content_actions::AssetRevisionStates>()?
        .state_of(&rel)
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Hooks the Content Browser, the Outliner and the status bar call
// ---------------------------------------------------------------------------

/// Content Browser list row: `text` with the path's revision letter appended.
#[must_use]
pub fn decorate_row(world: &World, rel: &str, text: String) -> String {
    let text = match state_of(world, rel) {
        Some(state) => format!("{text}   [{}]", state.letter()),
        None => text,
    };
    locks::decorate_row(world, rel, text)
}

/// Content Browser tile: the revision letter in the picture's top-left corner.
pub fn paint_tile_badge(ui: &egui::Ui, world: &World, rel: &str, picture: egui::Rect, font: &egui::FontId) {
    if let Some(state) = state_of(world, rel) {
        ui.painter().text(
            picture.left_top() + egui::vec2(4.0, 4.0),
            egui::Align2::LEFT_TOP,
            state.letter(),
            font.clone(),
            state.color(ui.ctx()),
        );
    }
    locks::paint_tile_badge(ui, world, rel, picture, font);
}

/// The Outliner's level row: the level file and its revision badge.
pub fn outliner_level_row(ui: &mut egui::Ui, world: &World) {
    let Some(rel) = level_path(world) else {
        return;
    };
    if cached(world).is_none() {
        return;
    }
    let name = rel.rsplit('/').next().unwrap_or(&rel).to_string();
    ui.horizontal(|ui| {
        ui.weak(format!("Level: {name}"));
        match level_state(world) {
            Some(state) => {
                ui.colored_label(state.color(ui.ctx()), state.letter())
                    .on_hover_text(state.describe());
            }
            None => {
                ui.weak("·").on_hover_text("Revision control: no changes");
            }
        }
    });
}

/// The status bar's revision slot: the branch and the number of changed paths; a click opens the
/// Revision Control window.
pub fn status_bar_slot(ui: &mut egui::Ui, world: &mut World) {
    let Some(state) = world.get_resource::<VcsStatus>().map(|s| s.state.clone()) else {
        return;
    };
    let Some((label, _)) = slot_texts(&state) else {
        return;
    };
    let text = match state {
        VcsState::Ready(_) => format!("\u{b7} \u{2387} {label}"),
        _ => format!("\u{b7} {label}"),
    };
    let slot = ui.small_button(text);
    if slot
        .on_hover_text("Revision Control (View \u{25b8} Revision Control)")
        .clicked()
    {
        panel::toggle(world);
    }
}
