// SPDX-License-Identifier: MPL-2.0
//! File locks for projects that use git LFS file locking (spec 19 addendum § 18.3, ADR-7864):
//! check out / lock on edit, lock badges, the save guard, unlock after commit and Unlock all mine.
//!
//! - **Detection.** Locking is available when the repository's attributes mark some paths
//!   `lockable` (`.gitattributes` at the repository top, in the project directory, and
//!   `.git/info/attributes`; [`LockRules`]) and `git lfs version` runs. Otherwise every call
//!   answers `locking_unavailable`, the badges stay off and nothing is ever blocked.
//! - **The lock list.** `git lfs locks --verify --json` (ours and theirs) through the git program of
//!   [`super::git_program`]; argument vectors only. The list is cached in [`VcsLocks`]: it is read in
//!   the background when the project (or the git program) changes, and synchronously on the first
//!   edit of a lockable file, on `editor.vcs_locks` and on the File Locks window's Refresh. Nothing
//!   runs per frame.
//! - **Lock on edit.** [`tick`] watches the documents with unsaved edits (every asset editor's
//!   [`crate::doc_autosave::dirty_documents`] and the open level). The first dirtying edit of a
//!   lockable file that is not locked by the user reads the lock list once, then: locked by
//!   someone else → a warning (saving will be blocked); otherwise the preference
//!   `vcs.lock_on_edit` decides: `ask` (default) shows the "Lock this file?" prompt, `always`
//!   locks it at once (`git lfs lock`), `never` does nothing.
//! - **Save guard.** [`guard_save`] refuses a save to a file someone else locks with
//!   `locked_by_other`; the level save (`level_io::save_level_and_terrain`) and the save-all path
//!   of the other documents (`Shell::save_other_documents`) call it. The level's prompt offers
//!   "Save anyway as a copy" (a new file next to it; the locked file stays untouched).
//! - **Unlock.** After a commit the committed files the user still locks are offered for unlock;
//!   "Unlock all mine" releases every lock of the user. Other users' locks are never forced.
//! - **Read-only marking.** With the preference `vcs.lockable_read_only`, lockable files not
//!   locked by the user show a read-only badge.
//! - **Agent surface.** `editor.vcs_locks`, `editor.vcs_lock`, `editor.vcs_unlock`
//!   (`book/agents/revision-control.md`).

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use super::{GitDriver, GitProgram, VcsError};
use crate::viewport_agent::Handler;

/// The preference that decides what the first edit of a lockable file does.
pub const LOCK_ON_EDIT_PREF: &str = "vcs.lock_on_edit";
/// The values of [`LOCK_ON_EDIT_PREF`].
pub const LOCK_ON_EDIT_CHOICES: &[&str] = &["ask", "always", "never"];
/// The preference that shows lockable files not locked by the user as read-only.
pub const READ_ONLY_PREF: &str = "vcs.lockable_read_only";

/// Every error code the lock methods answer besides the revision-control ones (the book page
/// lists them).
pub const ERROR_CODES: &[&str] = &[
    "locking_unavailable",
    "not_lockable",
    "locked_by_other",
    "not_locked_by_you",
    "lfs_failed",
];

// ---------------------------------------------------------------------------
// Lockable rules (`.gitattributes`)
// ---------------------------------------------------------------------------

/// One attributes line that sets or unsets `lockable`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LockRule {
    /// The directory of the attributes file, repository-relative, `/`-terminated or empty.
    base: String,
    /// The pattern as written (without a leading `/`).
    pattern: String,
    /// The pattern contains a `/`: it matches the path below `base`, else the file name.
    anchored: bool,
    /// `lockable` set (`true`) or unset (`-lockable`, `!lockable`: `false`).
    set: bool,
}

/// The `lockable` attribute rules of a repository, in precedence order (a later rule wins).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockRules {
    rules: Vec<LockRule>,
}

impl LockRules {
    /// Reads the rules that apply to a project: the repository top's `.gitattributes`, the
    /// project directory's own (when the project is a subdirectory) and `.git/info/attributes`.
    #[must_use]
    pub fn read(top: &Path, prefix: &str) -> Self {
        let mut rules = Self::default();
        let mut add = |file: PathBuf, base: &str| {
            if let Ok(text) = std::fs::read_to_string(&file) {
                rules.add_text(&text, base);
            }
        };
        add(top.join(".gitattributes"), "");
        if !prefix.is_empty() {
            add(top.join(prefix).join(".gitattributes"), prefix);
        }
        add(top.join(".git").join("info").join("attributes"), "");
        rules
    }

    /// Adds the `lockable` lines of one attributes file whose directory is `base`
    /// (repository-relative, `/`-terminated or empty).
    pub fn add_text(&mut self, text: &str, base: &str) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut words = line.split_whitespace();
            let Some(pattern) = words.next() else {
                continue;
            };
            // A directory pattern never matches a file in an attributes file; a negated pattern
            // is not allowed there.
            if pattern.ends_with('/') || pattern.starts_with('!') {
                continue;
            }
            for word in words {
                let set = match word {
                    "lockable" | "lockable=true" | "lockable=set" => true,
                    "-lockable" | "!lockable" | "lockable=false" => false,
                    _ => continue,
                };
                let anchored = pattern.contains('/');
                self.rules.push(LockRule {
                    base: base.to_string(),
                    pattern: pattern.trim_start_matches('/').to_string(),
                    anchored,
                    set,
                });
            }
        }
    }

    /// Whether any rule makes some path lockable.
    #[must_use]
    pub fn any(&self) -> bool {
        self.rules.iter().any(|r| r.set)
    }

    /// The patterns that set `lockable`, as written (for the protocol and the window).
    #[must_use]
    pub fn patterns(&self) -> Vec<String> {
        self.rules
            .iter()
            .filter(|r| r.set)
            .map(|r| format!("{}{}", r.base, r.pattern))
            .collect()
    }

    /// Whether the repository path `repo_path` (`/`-separated) is lockable: the last matching
    /// rule decides.
    #[must_use]
    pub fn is_lockable(&self, repo_path: &str) -> bool {
        let mut lockable = false;
        for rule in &self.rules {
            let Some(below) = repo_path.strip_prefix(rule.base.as_str()) else {
                continue;
            };
            let subject = if rule.anchored {
                below
            } else {
                below.rsplit('/').next().unwrap_or(below)
            };
            if glob_match(rule.pattern.as_bytes(), subject.as_bytes()) {
                lockable = rule.set;
            }
        }
        lockable
    }
}

/// Matches `text` against an attributes pattern: `*` (not across `/`), `?`, `[...]` classes
/// (`!`/`^` negation, ranges), `\` escapes, and `**` (`**/` any leading directories, `/**/` zero
/// or more directories, a trailing `/**` everything below).
#[must_use]
pub fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    let Some((&first, rest)) = pattern.split_first() else {
        return text.is_empty();
    };
    match first {
        b'*' if rest.first() == Some(&b'*') => {
            let after = &rest[1..];
            if let Some(tail) = after.strip_prefix(b"/") {
                // `**/`: zero or more whole directories.
                if glob_match(tail, text) {
                    return true;
                }
                text.iter()
                    .enumerate()
                    .any(|(i, &c)| c == b'/' && glob_match(tail, &text[i + 1..]))
            } else {
                (0..=text.len()).any(|i| glob_match(after, &text[i..]))
            }
        }
        b'*' => {
            for i in 0..=text.len() {
                if glob_match(rest, &text[i..]) {
                    return true;
                }
                if text.get(i) == Some(&b'/') {
                    break;
                }
            }
            false
        }
        b'?' => match text.split_first() {
            Some((&c, tail)) if c != b'/' => glob_match(rest, tail),
            _ => false,
        },
        b'[' => match class_match(rest, text.first().copied()) {
            Some((true, after)) => glob_match(after, &text[1..]),
            Some((false, _)) => false,
            // No closing bracket: a literal `[`.
            None => text.first() == Some(&b'[') && glob_match(rest, &text[1..]),
        },
        b'\\' if !rest.is_empty() => text.first() == Some(&rest[0]) && glob_match(&rest[1..], &text[1..]),
        literal => text.first() == Some(&literal) && glob_match(rest, &text[1..]),
    }
}

/// A `[...]` class after its `[`: whether `c` matches, and the pattern after the `]`. `None` when
/// the class is not closed.
fn class_match(class: &[u8], c: Option<u8>) -> Option<(bool, &[u8])> {
    let (negated, mut i) = match class.first() {
        Some(b'!' | b'^') => (true, 1),
        _ => (false, 0),
    };
    let mut hit = false;
    let mut first = true;
    while i < class.len() {
        let ch = class[i];
        if ch == b']' && !first {
            let ok = c.is_some_and(|c| c != b'/') && (hit != negated);
            return Some((ok, &class[i + 1..]));
        }
        first = false;
        if i + 2 < class.len() && class[i + 1] == b'-' && class[i + 2] != b']' {
            if c.is_some_and(|c| (ch..=class[i + 2]).contains(&c)) {
                hit = true;
            }
            i += 3;
        } else {
            if c == Some(ch) {
                hit = true;
            }
            i += 1;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The lock list
// ---------------------------------------------------------------------------

/// One lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockInfo {
    /// Project-relative path, `/`-separated.
    pub path: String,
    /// The lock id the LFS server gave.
    pub id: String,
    /// The owner's name as the LFS server reports it.
    pub owner: String,
    /// The user holds this lock.
    pub mine: bool,
    /// When it was taken (ISO 8601, as reported).
    pub locked_at: String,
}

impl LockInfo {
    /// The protocol form.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "id": self.id,
            "owner": self.owner,
            "mine": self.mine,
            "locked_at": self.locked_at,
        })
    }
}

/// Whether the project can use file locks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum LockSupport {
    /// Not checked yet.
    #[default]
    Unknown,
    /// No locking: the reason (no lockable patterns, git LFS missing, not a git work tree).
    Unavailable(String),
    /// Lockable patterns exist and git LFS runs.
    Available,
}

impl LockSupport {
    /// The protocol name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Unavailable(_) => "unavailable",
            Self::Available => "available",
        }
    }
}

/// The git LFS side of one repository: lock, unlock, list.
#[derive(Debug, Clone)]
pub struct LfsLocks {
    driver: GitDriver,
    program: GitProgram,
    /// The `lockable` rules of the repository.
    pub rules: LockRules,
}

/// What one read of the lock state found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockSnapshot {
    /// Available or not (with the reason).
    pub support: LockSupport,
    /// The rules (empty when unavailable for lack of a work tree).
    pub rules: LockRules,
    /// The locks, sorted by path (empty when unavailable).
    pub locks: Vec<LockInfo>,
    /// The project directory relative to the repository, `/`-terminated (empty at the top).
    pub prefix: String,
}

fn lfs_error(op: &str, error: &VcsError) -> VcsError {
    VcsError::new("lfs_failed", format!("git lfs {op}: {}", error.message))
}

impl LfsLocks {
    /// The lock side of the repository `project` is in, checked: lockable patterns exist and
    /// `git lfs version` runs.
    ///
    /// # Errors
    /// `no_project`, `no_revision_control`, `git_not_found`, `git_failed`, or
    /// `locking_unavailable` (with the reason).
    pub fn open(project: &Path, program: GitProgram) -> Result<Self, VcsError> {
        let driver = GitDriver::open(project, program.clone())?;
        let rules = LockRules::read(driver.top(), driver.prefix());
        if !rules.any() {
            return Err(VcsError::new(
                "locking_unavailable",
                "File locking is not available: no path is marked lockable in .gitattributes",
            ));
        }
        if program.run(driver.top(), &["lfs", "version"], None).is_err() {
            return Err(VcsError::new(
                "locking_unavailable",
                "File locking is not available: git LFS is not installed (git lfs version failed)",
            ));
        }
        Ok(Self {
            driver,
            program,
            rules,
        })
    }

    /// The project directory relative to the repository, `/`-terminated (empty at the top).
    #[must_use]
    pub fn prefix(&self) -> &str {
        self.driver.prefix()
    }

    fn repo_path(&self, project_path: &str) -> String {
        format!("{}{project_path}", self.driver.prefix())
    }

    fn project_path(&self, repo_path: &str) -> Option<String> {
        let repo_path = repo_path.replace('\\', "/");
        let prefix = self.driver.prefix();
        if prefix.is_empty() {
            return Some(repo_path);
        }
        repo_path
            .strip_prefix(prefix)
            .filter(|rest| !rest.is_empty())
            .map(str::to_string)
    }

    fn lfs(&self, args: &[&str]) -> Result<Vec<u8>, VcsError> {
        let mut all = vec!["lfs"];
        all.extend_from_slice(args);
        self.program.run(self.driver.top(), &all, None)
    }

    /// Whether the project path is lockable.
    ///
    /// # Errors
    /// `invalid_path`.
    pub fn is_lockable(&self, path: &str) -> Result<bool, VcsError> {
        let path = super::git::clean_project_path(path)?;
        Ok(self.rules.is_lockable(&self.repo_path(&path)))
    }

    /// Every lock of the repository inside the project (`git lfs locks --verify --json`), sorted.
    ///
    /// # Errors
    /// `lfs_failed`.
    pub fn list(&self) -> Result<Vec<LockInfo>, VcsError> {
        let _span = tracing::info_span!("vcs_lfs_locks").entered();
        let bytes = self
            .lfs(&["locks", "--verify", "--json"])
            .map_err(|e| lfs_error("locks", &e))?;
        let user = self
            .program
            .run(self.driver.top(), &["config", "user.name"], None)
            .map(|b| String::from_utf8_lossy(&b).trim().to_string())
            .unwrap_or_default();
        let mut locks: Vec<LockInfo> = parse_locks_json(&bytes, &user)?
            .into_iter()
            .filter_map(|mut lock| {
                lock.path = self.project_path(&lock.path)?;
                Some(lock)
            })
            .collect();
        locks.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(locks)
    }

    /// Locks one project path (`git lfs lock --json`).
    ///
    /// # Errors
    /// `invalid_path`, `not_lockable` or `lfs_failed`.
    pub fn lock(&self, path: &str) -> Result<LockInfo, VcsError> {
        let path = super::git::clean_project_path(path)?;
        let repo = self.repo_path(&path);
        if !self.rules.is_lockable(&repo) {
            return Err(VcsError::new(
                "not_lockable",
                format!("{path} is not marked lockable in .gitattributes"),
            ));
        }
        let bytes = self
            .lfs(&["lock", "--json", "--", repo.as_str()])
            .map_err(|e| lfs_error("lock", &e))?;
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let mut lock = lock_from_json(&value, true).unwrap_or(LockInfo {
            path: repo,
            id: String::new(),
            owner: String::new(),
            mine: true,
            locked_at: String::new(),
        });
        lock.path = path;
        lock.mine = true;
        Ok(lock)
    }

    /// Releases the user's lock of one project path (`git lfs unlock --json`; never `--force`).
    ///
    /// # Errors
    /// `invalid_path` or `lfs_failed`.
    pub fn unlock(&self, path: &str) -> Result<(), VcsError> {
        let path = super::git::clean_project_path(path)?;
        let repo = self.repo_path(&path);
        self.lfs(&["unlock", "--json", "--", repo.as_str()])
            .map_err(|e| lfs_error("unlock", &e))
            .map(|_| ())
    }
}

/// Reads one lock record (`{ id, path, owner: { name }, locked_at }`).
fn lock_from_json(value: &Value, mine: bool) -> Option<LockInfo> {
    Some(LockInfo {
        path: value.get("path")?.as_str()?.to_string(),
        id: match value.get("id") {
            Some(Value::String(s)) => s.clone(),
            Some(other) if !other.is_null() => other.to_string(),
            _ => String::new(),
        },
        owner: value
            .get("owner")
            .and_then(|o| o.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        mine,
        locked_at: value
            .get("locked_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// Parses `git lfs locks --verify --json` (`{ ours: [...], theirs: [...] }`, from the git-lfs
/// `git-lfs-locks` manual). A plain array (a server without verification) counts a lock as the
/// user's when its owner name is `user_name`.
///
/// # Errors
/// `lfs_failed` when the text is not one of those shapes.
pub fn parse_locks_json(bytes: &[u8], user_name: &str) -> Result<Vec<LockInfo>, VcsError> {
    let text = String::from_utf8_lossy(bytes);
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let value: Value = serde_json::from_str(text.trim()).map_err(|e| {
        VcsError::new(
            "lfs_failed",
            format!("git lfs locks answered unreadable JSON: {e}"),
        )
    })?;
    let side = |key: &str, mine: bool| -> Vec<LockInfo> {
        value
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| lock_from_json(v, mine))
            .collect()
    };
    match &value {
        Value::Object(_) => {
            let mut all = side("ours", true);
            all.extend(side("theirs", false));
            Ok(all)
        }
        Value::Array(items) => Ok(items
            .iter()
            .filter_map(|v| {
                let mut lock = lock_from_json(v, false)?;
                lock.mine = !user_name.is_empty() && lock.owner == user_name;
                Some(lock)
            })
            .collect()),
        _ => Err(VcsError::new(
            "lfs_failed",
            "git lfs locks answered an unexpected JSON shape",
        )),
    }
}

/// Reads the whole lock state of a project (the background refresh's work).
#[must_use]
pub fn read_snapshot(project: &Path, program: GitProgram) -> LockSnapshot {
    match LfsLocks::open(project, program) {
        Ok(lfs) => match lfs.list() {
            Ok(locks) => LockSnapshot {
                support: LockSupport::Available,
                prefix: lfs.prefix().to_string(),
                rules: lfs.rules,
                locks,
            },
            Err(error) => {
                tracing::warn!(target: "regia_editor", "file locks: {error}");
                LockSnapshot {
                    support: LockSupport::Available,
                    prefix: lfs.prefix().to_string(),
                    rules: lfs.rules,
                    locks: Vec::new(),
                }
            }
        },
        Err(error) => LockSnapshot::unavailable(error.message),
    }
}

impl LockSnapshot {
    fn unavailable(reason: String) -> Self {
        Self {
            support: LockSupport::Unavailable(reason),
            rules: LockRules::default(),
            locks: Vec::new(),
            prefix: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// The editor-world cache, prompts and the lock-on-edit watcher
// ---------------------------------------------------------------------------

/// A question the File Locks prompt asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockPrompt {
    /// The first edit of an unlocked lockable file (`vcs.lock_on_edit = ask`): lock it?
    LockOnEdit(String),
    /// The edited file is locked by someone else: saving it will be blocked.
    LockedByOther {
        /// Project path.
        path: String,
        /// The lock owner's name.
        owner: String,
    },
    /// A level save was refused: the level is locked by someone else. Offers a copy.
    SaveBlocked {
        /// The level file (absolute).
        path: PathBuf,
        /// The lock owner's name.
        owner: String,
    },
}

impl LockPrompt {
    /// The project path (or file) the prompt is about.
    #[must_use]
    pub fn subject(&self) -> String {
        match self {
            Self::LockOnEdit(path) | Self::LockedByOther { path, .. } => path.clone(),
            Self::SaveBlocked { path, .. } => path.display().to_string(),
        }
    }

    /// The protocol form.
    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            Self::LockOnEdit(path) => json!({ "kind": "lock_on_edit", "path": path }),
            Self::LockedByOther { path, owner } => {
                json!({ "kind": "locked_by_other", "path": path, "owner": owner })
            }
            Self::SaveBlocked { path, owner } => json!({
                "kind": "save_blocked",
                "path": path.display().to_string(),
                "owner": owner,
            }),
        }
    }
}

/// What a prompt or window control asked for; applied after the draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockOp {
    /// Read the lock list now.
    Refresh,
    /// Lock these project paths.
    Lock(Vec<String>),
    /// Unlock these project paths.
    Unlock(Vec<String>),
    /// Unlock every lock of the user.
    UnlockAllMine,
    /// Close the first prompt without acting.
    Dismiss,
    /// Save the blocked level as a copy next to it.
    SaveCopy,
}

/// The lock cache (an editor-world resource).
#[derive(Resource, Debug, Default)]
pub struct VcsLocks {
    /// Available or not.
    pub support: LockSupport,
    /// The `lockable` rules.
    pub rules: LockRules,
    /// The project directory relative to the repository, `/`-terminated (empty at the top).
    pub prefix: String,
    /// Every lock in the project, sorted by path.
    pub locks: Vec<LockInfo>,
    /// `git lfs lock` calls made.
    pub lock_calls: u64,
    /// `git lfs unlock` calls made.
    pub unlock_calls: u64,
    /// Lock-list reads (background and synchronous).
    pub reads: u64,
    /// Open questions, oldest first.
    pub prompts: VecDeque<LockPrompt>,
    /// Committed files the user still locks (offered for unlock after a commit).
    pub unlock_offer: Vec<String>,
    /// The File Locks window is shown.
    pub window_open: bool,
    /// The last result line (`true`: an error).
    pub notice: Option<(bool, String)>,
    /// Ops queued by this frame's draw.
    pub pending: Vec<LockOp>,
    /// Ops applied.
    pub ops_applied: u64,
    applied_frame: Option<u64>,
    key: Option<(Option<PathBuf>, GitProgram, u64)>,
    requests: u64,
    dirty_seen: BTreeSet<String>,
    inflight: Option<Mutex<Receiver<LockSnapshot>>>,
}

impl VcsLocks {
    /// The lock of a project path.
    #[must_use]
    pub fn lock_of(&self, path: &str) -> Option<&LockInfo> {
        self.locks
            .binary_search_by(|l| l.path.as_str().cmp(path))
            .ok()
            .and_then(|i| self.locks.get(i))
    }

    /// Whether locking works in this project.
    #[must_use]
    pub fn available(&self) -> bool {
        self.support == LockSupport::Available
    }

    /// Whether the project path is lockable (`false` when locking is unavailable).
    #[must_use]
    pub fn is_lockable(&self, path: &str) -> bool {
        self.available() && self.rules.is_lockable(&format!("{}{path}", self.prefix))
    }

    fn apply_snapshot(&mut self, snapshot: LockSnapshot) {
        self.reads += 1;
        self.support = snapshot.support;
        self.rules = snapshot.rules;
        self.locks = snapshot.locks;
        self.prefix = snapshot.prefix;
    }

    /// Marks locking available with `lfs`'s rules (a lock call that found it so before any read).
    fn adopt(&mut self, lfs: &LfsLocks) {
        if !self.available() {
            self.support = LockSupport::Available;
            self.rules = lfs.rules.clone();
            self.prefix = lfs.prefix().to_string();
        }
    }

    fn upsert(&mut self, lock: LockInfo) {
        match self.locks.binary_search_by(|l| l.path.cmp(&lock.path)) {
            Ok(i) => self.locks[i] = lock,
            Err(i) => self.locks.insert(i, lock),
        }
    }

    fn push_prompt(&mut self, prompt: LockPrompt) {
        if !self.prompts.contains(&prompt) {
            self.prompts.push_back(prompt);
        }
    }
}

/// The lock side of the open project.
///
/// # Errors
/// See [`LfsLocks::open`].
pub fn lfs(world: &World) -> Result<LfsLocks, VcsError> {
    let root = super::project_root(world).ok_or_else(|| VcsError::new("no_project", "no project is open"))?;
    LfsLocks::open(&root, super::git_program(world))
}

/// Reads the lock state now on this thread and stores it.
pub fn refresh_now(world: &mut World) -> LockSupport {
    let snapshot = match super::project_root(world) {
        Some(root) => read_snapshot(&root, super::git_program(world)),
        None => LockSnapshot::unavailable("no project is open".to_string()),
    };
    let support = snapshot.support.clone();
    let mut locks = world.get_resource_or_insert_with(VcsLocks::default);
    locks.apply_snapshot(snapshot);
    support
}

/// Asks for a background read on the next tick.
pub fn request_refresh(world: &mut World) {
    world.get_resource_or_insert_with(VcsLocks::default).requests += 1;
}

/// The project paths with unsaved edits: every asset editor's dirty documents and the open level.
#[must_use]
pub fn edited_paths(world: &World) -> BTreeSet<String> {
    let Some(root) = super::project_root(world) else {
        return BTreeSet::new();
    };
    let rel = |path: &Path| -> Option<String> {
        crate::path_norm::strip_prefix_fold(&root, path, crate::path_norm::FOLD_CASE)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .filter(|p| !p.is_empty())
    };
    let mut out: BTreeSet<String> = crate::doc_autosave::dirty_documents(world)
        .into_iter()
        .filter_map(|key| key.original.as_deref().and_then(rel))
        .collect();
    let level_dirty = world
        .get_resource::<crate::command::UndoStack>()
        .is_some_and(crate::command::UndoStack::is_dirty);
    if level_dirty
        && let Some(path) = world
            .get_resource::<crate::level_io::OpenLevel>()
            .and_then(|open| open.path.clone())
        && let Some(path) = rel(&path)
    {
        out.insert(path);
    }
    out
}

/// One tick: take a finished background read, start one when the project changed, and run the
/// lock-on-edit watcher (only while locking is available; nothing runs otherwise).
pub fn tick(world: &mut World) {
    tick_with(world, false);
}

/// [`tick`], then waits for a background read in flight (tests and agents).
pub fn tick_and_wait(world: &mut World) {
    tick_with(world, true);
}

fn tick_with(world: &mut World, block: bool) {
    let root = super::project_root(world);
    let program = super::git_program(world);
    let mut locks = world.remove_resource::<VcsLocks>().unwrap_or_default();
    receive(&mut locks, false);
    let key = (root.clone(), program.clone(), locks.requests);
    if locks.inflight.is_none() && locks.key.as_ref() != Some(&key) {
        match root {
            None => {
                locks.support = LockSupport::Unavailable("no project is open".to_string());
                locks.locks.clear();
            }
            Some(root) => {
                let (tx, rx) = channel();
                let spawned = std::thread::Builder::new()
                    .name("vcs-locks".into())
                    .spawn(move || {
                        let _ = tx.send(read_snapshot(&root, program));
                    });
                match spawned {
                    Ok(_) => locks.inflight = Some(Mutex::new(rx)),
                    Err(e) => {
                        locks.support =
                            LockSupport::Unavailable(format!("could not start the lock reader: {e}"));
                    }
                }
            }
        }
        locks.key = Some(key);
    }
    if block {
        receive(&mut locks, true);
    }
    let available = locks.available();
    world.insert_resource(locks);
    if available {
        watch_edits(world);
    }
}

fn receive(locks: &mut VcsLocks, block: bool) {
    let Some(rx) = locks.inflight.take() else {
        return;
    };
    let answer = {
        let Ok(guard) = rx.lock() else {
            return;
        };
        if block {
            guard.recv().map_err(|_| TryRecvError::Disconnected)
        } else {
            guard.try_recv()
        }
    };
    match answer {
        Ok(snapshot) => locks.apply_snapshot(snapshot),
        Err(TryRecvError::Empty) => locks.inflight = Some(rx),
        Err(TryRecvError::Disconnected) => {
            locks.support = LockSupport::Unavailable("the lock reader stopped without an answer".to_string());
        }
    }
}

/// The lock-on-edit watcher: acts once per lockable file each time it turns dirty.
fn watch_edits(world: &mut World) {
    let _span = tracing::debug_span!("vcs_lock_on_edit").entered();
    let dirty = edited_paths(world);
    let fresh: Vec<String> = {
        let Some(locks) = world.get_resource::<VcsLocks>() else {
            return;
        };
        dirty
            .iter()
            .filter(|p| !locks.dirty_seen.contains(*p))
            .filter(|p| locks.is_lockable(p))
            .filter(|p| !locks.lock_of(p).is_some_and(|l| l.mine))
            .cloned()
            .collect()
    };
    if let Some(mut locks) = world.get_resource_mut::<VcsLocks>() {
        locks.dirty_seen = dirty;
    }
    if fresh.is_empty() {
        return;
    }
    // One read of the lock list: someone may hold the file now.
    refresh_now(world);
    let mode = world
        .get_resource::<crate::editor_prefs::EditorPreferences>()
        .map_or_else(|| "ask".to_string(), |p| p.text(LOCK_ON_EDIT_PREF));
    for path in fresh {
        let held = world
            .get_resource::<VcsLocks>()
            .and_then(|l| l.lock_of(&path).cloned());
        match held {
            Some(lock) if lock.mine => {}
            Some(lock) => {
                let mut locks = world.get_resource_or_insert_with(VcsLocks::default);
                locks.notice = Some((
                    true,
                    format!("{path} is locked by {}; saving it will be blocked", lock.owner),
                ));
                locks.push_prompt(LockPrompt::LockedByOther {
                    path,
                    owner: lock.owner,
                });
            }
            None => match mode.as_str() {
                "always" => {
                    let result = lock_paths(world, std::slice::from_ref(&path));
                    let mut locks = world.get_resource_or_insert_with(VcsLocks::default);
                    locks.notice = Some(match result {
                        Ok(_) => (false, format!("Locked {path} for editing")),
                        Err(error) => (true, error.message),
                    });
                }
                "never" => {}
                _ => world
                    .get_resource_or_insert_with(VcsLocks::default)
                    .push_prompt(LockPrompt::LockOnEdit(path)),
            },
        }
    }
}

/// Locks project paths; answers the locks taken. A path someone else holds answers
/// `locked_by_other`.
///
/// # Errors
/// `locking_unavailable`, `invalid_path`, `not_lockable`, `locked_by_other` or `lfs_failed`.
pub fn lock_paths(world: &mut World, paths: &[String]) -> Result<Vec<LockInfo>, VcsError> {
    if paths.is_empty() {
        return Err(VcsError::new("invalid_path", "no paths given"));
    }
    let lfs = lfs(world)?;
    world.get_resource_or_insert_with(VcsLocks::default).adopt(&lfs);
    let mut taken = Vec::new();
    for path in paths {
        let path = super::git::clean_project_path(path)?;
        if !lfs.is_lockable(&path)? {
            return Err(VcsError::new(
                "not_lockable",
                format!("{path} is not marked lockable in .gitattributes"),
            ));
        }
        let known = world
            .get_resource::<VcsLocks>()
            .and_then(|l| l.lock_of(&path).cloned());
        if let Some(lock) = known {
            if lock.mine {
                taken.push(lock);
                continue;
            }
            return Err(locked_by(&path, &lock.owner));
        }
        world.get_resource_or_insert_with(VcsLocks::default).lock_calls += 1;
        match lfs.lock(&path) {
            Ok(lock) => {
                let mut locks = world.get_resource_or_insert_with(VcsLocks::default);
                locks.upsert(lock.clone());
                locks
                    .prompts
                    .retain(|p| p != &LockPrompt::LockOnEdit(path.clone()));
                taken.push(lock);
            }
            Err(error) => {
                // The server refused: find out whether someone else holds it.
                refresh_now(world);
                if let Some(lock) = world
                    .get_resource::<VcsLocks>()
                    .and_then(|l| l.lock_of(&path).cloned())
                    .filter(|l| !l.mine)
                {
                    return Err(locked_by(&path, &lock.owner));
                }
                return Err(error);
            }
        }
    }
    Ok(taken)
}

fn locked_by(path: &str, owner: &str) -> VcsError {
    VcsError::new(
        "locked_by_other",
        format!(
            "{path} is locked by {owner}; saving it is blocked (save a copy, or ask {owner} to unlock it)"
        ),
    )
}

/// Releases the user's locks of project paths; answers the paths unlocked.
///
/// # Errors
/// `locking_unavailable`, `invalid_path`, `not_locked_by_you` or `lfs_failed`.
pub fn unlock_paths(world: &mut World, paths: &[String]) -> Result<Vec<String>, VcsError> {
    if paths.is_empty() {
        return Err(VcsError::new("invalid_path", "no paths given"));
    }
    let lfs = lfs(world)?;
    world.get_resource_or_insert_with(VcsLocks::default).adopt(&lfs);
    let mut done = Vec::new();
    for path in paths {
        let path = super::git::clean_project_path(path)?;
        let known = world
            .get_resource::<VcsLocks>()
            .and_then(|l| l.lock_of(&path).cloned());
        match known {
            Some(lock) if lock.mine => {}
            Some(lock) => {
                return Err(VcsError::new(
                    "not_locked_by_you",
                    format!(
                        "{path} is locked by {}, not by you; it is never force-unlocked",
                        lock.owner
                    ),
                ));
            }
            None => {
                return Err(VcsError::new(
                    "not_locked_by_you",
                    format!("{path} is not locked by you"),
                ));
            }
        }
        world.get_resource_or_insert_with(VcsLocks::default).unlock_calls += 1;
        lfs.unlock(&path)?;
        let mut locks = world.get_resource_or_insert_with(VcsLocks::default);
        locks.locks.retain(|l| l.path != path);
        locks.unlock_offer.retain(|p| p != &path);
        done.push(path);
    }
    Ok(done)
}

/// Releases every lock of the user; answers the paths unlocked.
///
/// # Errors
/// See [`unlock_paths`].
pub fn unlock_all_mine(world: &mut World) -> Result<Vec<String>, VcsError> {
    let mine: Vec<String> = world
        .get_resource::<VcsLocks>()
        .map(|l| {
            l.locks
                .iter()
                .filter(|l| l.mine)
                .map(|l| l.path.clone())
                .collect()
        })
        .unwrap_or_default();
    if mine.is_empty() {
        lfs(world)?;
        return Ok(Vec::new());
    }
    unlock_paths(world, &mine)
}

/// After a commit: the committed files the user still locks are offered for unlock.
pub fn note_committed(world: &mut World, committed: &[String]) {
    let Some(mut locks) = world.get_resource_mut::<VcsLocks>() else {
        return;
    };
    let offer: Vec<String> = committed
        .iter()
        .filter(|p| locks.lock_of(p).is_some_and(|l| l.mine))
        .cloned()
        .collect();
    if !offer.is_empty() {
        locks.notice = Some((
            false,
            format!(
                "Committed {} file(s) you lock: unlock them when you are done",
                offer.len()
            ),
        ));
        locks.window_open = true;
    }
    locks.unlock_offer = offer;
}

/// The save guard: refuses a save to a file someone else locks (`locked_by_other`). Uses the
/// cached lock list; never blocks when locking is unavailable or the file is outside the project.
///
/// # Errors
/// `locked_by_other`.
pub fn guard_save(world: &World, path: &Path) -> Result<(), VcsError> {
    match blocking_lock(world, path) {
        Some((rel, lock)) => Err(locked_by(&rel, &lock.owner)),
        None => Ok(()),
    }
}

/// The lock of someone else on the file `path` (with its project path), from the cache.
fn blocking_lock(world: &World, path: &Path) -> Option<(String, LockInfo)> {
    let locks = world.get_resource::<VcsLocks>()?;
    if !locks.available() || locks.locks.is_empty() {
        return None;
    }
    let root = super::project_root(world)?;
    let rel = crate::path_norm::strip_prefix_fold(&root, path, crate::path_norm::FOLD_CASE)?
        .to_string_lossy()
        .replace('\\', "/");
    let lock = locks.lock_of(&rel).filter(|l| !l.mine)?.clone();
    Some((rel, lock))
}

/// [`guard_save`] for the level save a user asked for: a refusal also opens the prompt that
/// offers "Save anyway as a copy". The error text carries the `locked_by_other` code.
///
/// # Errors
/// The refusal's text.
pub fn guard_level_save(world: &mut World, path: &Path) -> Result<(), String> {
    let Some((rel, lock)) = blocking_lock(world, path) else {
        return Ok(());
    };
    let error = locked_by(&rel, &lock.owner);
    world
        .get_resource_or_insert_with(VcsLocks::default)
        .push_prompt(LockPrompt::SaveBlocked {
            path: path.to_path_buf(),
            owner: lock.owner,
        });
    Err(error.to_string())
}

/// The save guard for the save-all path of the asset editors' documents: every dirty document
/// whose file someone else locks refuses the save.
///
/// # Errors
/// The first refusal's text (with the `locked_by_other` code).
pub fn guard_documents(world: &World) -> Result<(), String> {
    for key in crate::doc_autosave::dirty_documents(world) {
        if let Some(path) = key.original.as_deref() {
            guard_save(world, path).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// A free name for a copy next to `path`: `<name> (copy).<extensions>`, then `(copy 2)`, ...
#[must_use]
pub fn copy_path(path: &Path) -> PathBuf {
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (stem, ext) = match name.find('.') {
        Some(0) | None => (name.as_str(), ""),
        Some(dot) => name.split_at(dot),
    };
    (1u32..)
        .map(|n| {
            let tag = if n == 1 {
                " (copy)".to_string()
            } else {
                format!(" (copy {n})")
            };
            dir.join(format!("{stem}{tag}{ext}"))
        })
        .find(|candidate| !candidate.exists())
        .unwrap_or_else(|| path.with_extension("copy"))
}

/// Saves the open level as a copy next to the blocked file ("Save anyway as a copy"); the
/// locked file and the open level's path stay as they are. Answers the copy's path.
///
/// # Errors
/// When no level is open or the write fails.
pub fn save_level_copy(world: &mut World, blocked: &Path) -> Result<PathBuf, String> {
    let open = world
        .get_resource::<crate::level_io::OpenLevel>()
        .cloned()
        .ok_or("no level is open")?;
    let copy = copy_path(blocked);
    crate::level_io::save_level_file(world, &open, &copy)?;
    Ok(copy)
}

// ---------------------------------------------------------------------------
// Badges (the Content Browser rows and tiles)
// ---------------------------------------------------------------------------

/// The lock badge of a project path: `(letter, hover)`. `L` is the user's lock, `X` someone
/// else's, `RO` a lockable file nobody locks when read-only marking is on.
#[must_use]
pub fn badge(world: &World, rel: &str) -> Option<(&'static str, String)> {
    let locks = world.get_resource::<VcsLocks>()?;
    if !locks.available() {
        return None;
    }
    match locks.lock_of(rel) {
        Some(lock) if lock.mine => Some(("L", "Locked by you".to_string())),
        Some(lock) => Some(("X", format!("Locked by {}: saving is blocked", lock.owner))),
        None => {
            let read_only = world
                .get_resource::<crate::editor_prefs::EditorPreferences>()
                .is_some_and(|p| p.bool(READ_ONLY_PREF));
            (read_only && locks.is_lockable(rel))
                .then(|| ("RO", "Read-only until you lock it (lockable)".to_string()))
        }
    }
}

/// Content Browser list row: the lock badge appended.
#[must_use]
pub fn decorate_row(world: &World, rel: &str, text: String) -> String {
    match badge(world, rel) {
        Some((_, hover)) => format!("{text}   [{hover}]"),
        None => text,
    }
}

/// Content Browser tile: the lock letter in the picture's top-right corner.
pub fn paint_tile_badge(ui: &egui::Ui, world: &World, rel: &str, picture: egui::Rect, font: &egui::FontId) {
    if let Some((letter, _)) = badge(world, rel) {
        let tokens = crate::theme::ui_tokens(ui.ctx());
        let color = if letter == "X" {
            tokens.accents.error_text
        } else {
            tokens.accents.log_info
        };
        ui.painter().text(
            picture.right_top() + egui::vec2(-4.0, 4.0),
            egui::Align2::RIGHT_TOP,
            letter,
            font.clone(),
            color,
        );
    }
}

/// The Content Browser's context-menu items of a lockable file: Lock or Unlock.
pub fn context_menu_items(ui: &mut egui::Ui, world: &mut World, rel: &str) {
    let Some(locks) = world.get_resource::<VcsLocks>() else {
        return;
    };
    if !locks.is_lockable(rel) {
        return;
    }
    let mine = locks.lock_of(rel).map(|l| l.mine);
    let mut op = None;
    match mine {
        Some(true) => {
            if ui.button("Unlock").clicked() {
                op = Some(LockOp::Unlock(vec![rel.to_string()]));
            }
        }
        Some(false) => {
            ui.weak("Locked by someone else");
        }
        None => {
            if ui.button("Lock").clicked() {
                op = Some(LockOp::Lock(vec![rel.to_string()]));
            }
        }
    }
    if let Some(op) = op {
        let mut locks = world.get_resource_or_insert_with(VcsLocks::default);
        locks.pending.push(op);
        locks.window_open = true;
        ui.close();
    }
}

// ---------------------------------------------------------------------------
// The File Locks window and the prompt
// ---------------------------------------------------------------------------

/// Shows or hides the File Locks window (palette `view.toggle_file_locks`).
pub fn toggle(world: &mut World) -> bool {
    let open = {
        let mut locks = world.get_resource_or_insert_with(VcsLocks::default);
        locks.window_open = !locks.window_open;
        locks.window_open
    };
    if open {
        request_refresh(world);
    }
    open
}

/// Runs the queued ops through the shared functions.
pub fn apply(world: &mut World) {
    let ops = world
        .get_resource_mut::<VcsLocks>()
        .map(|mut l| std::mem::take(&mut l.pending))
        .unwrap_or_default();
    for op in ops {
        let result: Result<Option<String>, VcsError> = match op {
            LockOp::Refresh => {
                refresh_now(world);
                Ok(None)
            }
            LockOp::Lock(paths) => {
                lock_paths(world, &paths).map(|l| Some(format!("Locked {} file(s)", l.len())))
            }
            LockOp::Unlock(paths) => {
                unlock_paths(world, &paths).map(|d| Some(format!("Unlocked {} file(s)", d.len())))
            }
            LockOp::UnlockAllMine => {
                unlock_all_mine(world).map(|d| Some(format!("Unlocked {} file(s)", d.len())))
            }
            LockOp::Dismiss => {
                if let Some(mut locks) = world.get_resource_mut::<VcsLocks>() {
                    locks.prompts.pop_front();
                }
                Ok(None)
            }
            LockOp::SaveCopy => {
                let blocked = world
                    .get_resource::<VcsLocks>()
                    .and_then(|l| match l.prompts.front() {
                        Some(LockPrompt::SaveBlocked { path, .. }) => Some(path.clone()),
                        _ => None,
                    });
                match blocked {
                    Some(path) => {
                        if let Some(mut locks) = world.get_resource_mut::<VcsLocks>() {
                            locks.prompts.pop_front();
                        }
                        save_level_copy(world, &path)
                            .map(|copy| Some(format!("Saved a copy: {}", copy.display())))
                            .map_err(|e| VcsError::new("save_failed", e))
                    }
                    None => Ok(None),
                }
            }
        };
        let mut locks = world.get_resource_or_insert_with(VcsLocks::default);
        locks.ops_applied += 1;
        match result {
            Ok(Some(text)) => locks.notice = Some((false, text)),
            Ok(None) => {}
            Err(error) => locks.notice = Some((true, error.message)),
        }
    }
}

/// The prompt (when a question is open) and the File Locks window (when shown), drawn over the
/// dock; the ops they queue run once per frame.
pub fn window(ctx: &egui::Context, world: &mut World) {
    let (prompt, open) = match world.get_resource::<VcsLocks>() {
        Some(locks) => (locks.prompts.front().cloned(), locks.window_open),
        None => return,
    };
    if prompt.is_none() && !open {
        return;
    }
    let _span = tracing::info_span!("vcs_locks_window").entered();
    if let Some(prompt) = prompt {
        egui::Window::new("File lock")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| draw_prompt_and_apply(ui, world, &prompt));
    }
    if open {
        let mut still_open = true;
        egui::Window::new("File Locks")
            .open(&mut still_open)
            .default_width(460.0)
            .show(ctx, |ui| draw_and_apply(ui, world));
        if !still_open && let Some(mut locks) = world.get_resource_mut::<VcsLocks>() {
            locks.window_open = false;
        }
    }
}

fn apply_once(ui: &egui::Ui, world: &mut World) {
    let frame = ui.ctx().cumulative_frame_nr();
    let Some(mut locks) = world.get_resource_mut::<VcsLocks>() else {
        return;
    };
    if locks.applied_frame == Some(frame) {
        // A second UI pass in the same frame repeats the first pass's clicks.
        locks.pending.clear();
        return;
    }
    if locks.pending.is_empty() {
        return;
    }
    locks.applied_frame = Some(frame);
    apply(world);
}

/// Draws the first open prompt into `ui` and applies what it queued (once per frame).
pub fn draw_prompt_and_apply(ui: &mut egui::Ui, world: &mut World, prompt: &LockPrompt) {
    let mut op = None;
    match prompt {
        LockPrompt::LockOnEdit(path) => {
            ui.label(format!(
                "{path} is lockable and you are editing it. Lock it so nobody else changes it meanwhile?"
            ));
            ui.horizontal(|ui| {
                if ui.button("Lock").clicked() {
                    op = Some(LockOp::Lock(vec![path.clone()]));
                }
                if ui.button("Not now").clicked() {
                    op = Some(LockOp::Dismiss);
                }
            });
        }
        LockPrompt::LockedByOther { path, owner } => {
            ui.colored_label(
                crate::theme::ui_tokens(ui.ctx()).accents.error_text,
                format!("{path} is locked by {owner}. Saving it will be blocked until {owner} unlocks it."),
            );
            if ui.button("OK").clicked() {
                op = Some(LockOp::Dismiss);
            }
        }
        LockPrompt::SaveBlocked { path, owner } => {
            ui.colored_label(
                crate::theme::ui_tokens(ui.ctx()).accents.error_text,
                format!("{} is locked by {owner}; the save was blocked.", path.display()),
            );
            ui.horizontal(|ui| {
                if ui.button("Save anyway as a copy").clicked() {
                    op = Some(LockOp::SaveCopy);
                }
                if ui.button("Cancel").clicked() {
                    op = Some(LockOp::Dismiss);
                }
            });
        }
    }
    if let Some(op) = op {
        world
            .get_resource_or_insert_with(VcsLocks::default)
            .pending
            .push(op);
    }
    apply_once(ui, world);
}

/// Draws the File Locks window's contents into `ui` and applies what it queued (once per frame).
pub fn draw_and_apply(ui: &mut egui::Ui, world: &mut World) {
    let Some(locks) = world.get_resource::<VcsLocks>() else {
        return;
    };
    let support = locks.support.clone();
    let rows = locks.locks.clone();
    let offer = locks.unlock_offer.clone();
    let notice = locks.notice.clone();
    let mut ops = Vec::new();
    match &support {
        LockSupport::Available => {
            ui.weak(format!("Lockable: {}", locks.rules.patterns().join(", ")));
        }
        LockSupport::Unavailable(reason) => {
            ui.label(reason.as_str());
        }
        LockSupport::Unknown => {
            ui.label("Reading the file locks…");
        }
    }
    ui.horizontal(|ui| {
        if ui.button("Refresh").clicked() {
            ops.push(LockOp::Refresh);
        }
        let any_mine = rows.iter().any(|l| l.mine);
        if ui
            .add_enabled(any_mine, egui::Button::new("Unlock all mine"))
            .clicked()
        {
            ops.push(LockOp::UnlockAllMine);
        }
    });
    if !offer.is_empty() {
        ui.separator();
        ui.label(format!("Committed and still locked by you: {}", offer.join(", ")));
        if ui.button("Unlock committed files").clicked() {
            ops.push(LockOp::Unlock(offer.clone()));
        }
    }
    ui.separator();
    if support == LockSupport::Available && rows.is_empty() {
        ui.weak("No file is locked.");
    }
    for lock in &rows {
        ui.horizontal(|ui| {
            let who = if lock.mine {
                "you".to_string()
            } else {
                lock.owner.clone()
            };
            ui.label(format!("{}: locked by {who}", lock.path));
            if lock.mine && ui.button(format!("Unlock {}", lock.path)).clicked() {
                ops.push(LockOp::Unlock(vec![lock.path.clone()]));
            }
        });
    }
    if let Some((error, text)) = notice {
        ui.separator();
        if error {
            ui.colored_label(crate::theme::ui_tokens(ui.ctx()).accents.error_text, text);
        } else {
            ui.weak(text);
        }
    }
    world
        .get_resource_or_insert_with(VcsLocks::default)
        .pending
        .extend(ops);
    apply_once(ui, world);
}

// ---------------------------------------------------------------------------
// Agent surface
// ---------------------------------------------------------------------------

const PATHS_PARAM: ParamSchema = ParamSchema {
    name: "paths",
    kind: "array",
    required: true,
    description: "Project-relative paths (`/`-separated).",
};

/// `editor.vcs_locks`, `editor.vcs_lock` and `editor.vcs_unlock`.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.vcs_locks",
                tool: "vcs_locks",
                description: "File locks (git LFS locking): whether locking is available (lockable patterns in .gitattributes and git lfs installed; otherwise support is unavailable with the reason and nothing is ever blocked), every lock with its owner and whether it is yours, the open prompts, the files offered for unlock after a commit, and per-path answers for paths. Reads the lock list now unless refresh is false.",
                params: &[
                    ParamSchema {
                        name: "paths",
                        kind: "array",
                        required: false,
                        description: "Paths to answer about: lockable, locked, owner, mine, read_only.",
                    },
                    ParamSchema {
                        name: "refresh",
                        kind: "boolean",
                        required: false,
                        description: "Read the lock list from git lfs first (default true).",
                    },
                ],
                result: "{ support, reason, patterns: [string], locks: [{ path, id, owner, mine, locked_at }], mine: [path], theirs: [path], unlock_offer: [path], prompts: [{ kind, path, owner }], lock_calls, unlock_calls, query: [{ path, lockable, locked, owner, mine, read_only }] }",
                mutating: false,
                permission: Permission::Read,
            },
            locks_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.vcs_lock",
                tool: "vcs_lock",
                description: "Locks lockable files (git lfs lock): the Content Browser's Lock and the lock-on-edit prompt's Lock. A file someone else locks answers locked_by_other; a file you already lock is answered without a new call.",
                params: &[PATHS_PARAM],
                result: "{ locked: [{ path, id, owner, mine, locked_at }] }",
                mutating: true,
                permission: Permission::Assets,
            },
            lock_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.vcs_unlock",
                tool: "vcs_unlock",
                description: "Releases your locks (git lfs unlock; other users' locks are never forced): paths, or all_mine: true for Unlock all mine.",
                params: &[
                    ParamSchema {
                        name: "paths",
                        kind: "array",
                        required: false,
                        description: "Paths you lock.",
                    },
                    ParamSchema {
                        name: "all_mine",
                        kind: "boolean",
                        required: false,
                        description: "Unlock every file you lock.",
                    },
                ],
                result: "{ unlocked: [path] }",
                mutating: true,
                permission: Permission::Assets,
            },
            unlock_method as Handler,
        ),
    ]
}

/// The protocol error of a lock failure (the revision-control codes map as in
/// [`super::agent::rpc_error`]).
#[must_use]
pub fn rpc_error(error: &VcsError) -> RpcError {
    let (code, hint) = match error.code {
        "locking_unavailable" => (
            UNSUPPORTED,
            "mark files lockable in .gitattributes and install git LFS; without it nothing is locked or blocked",
        ),
        "not_lockable" => (INVALID_PARAMS, "editor.vcs_locks lists the lockable patterns"),
        "locked_by_other" => (
            UNSUPPORTED,
            "ask the owner to unlock it, or save a copy under another path",
        ),
        "not_locked_by_you" => (NOT_FOUND, "editor.vcs_locks lists your locks (mine)"),
        "lfs_failed" => (UNSUPPORTED, "see the message (git lfs's own error)"),
        _ => return super::agent::rpc_error(error),
    };
    RpcError::new(code, error.code, error.message.clone(), hint)
}

fn paths_of(params: &Value) -> Result<Vec<String>, VcsError> {
    let items = params
        .get("paths")
        .and_then(Value::as_array)
        .ok_or_else(|| VcsError::new("invalid_path", "`paths` (array of project paths) is required"))?;
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| VcsError::new("invalid_path", format!("{item} is not a path string")))
        })
        .collect()
}

/// The lock state as the protocol answers it.
#[must_use]
pub fn locks_json(world: &World, query: &[String]) -> Value {
    let Some(locks) = world.get_resource::<VcsLocks>() else {
        return json!({ "support": "unknown" });
    };
    let reason = match &locks.support {
        LockSupport::Unavailable(reason) => Value::String(reason.clone()),
        _ => Value::Null,
    };
    let read_only = world
        .get_resource::<crate::editor_prefs::EditorPreferences>()
        .is_some_and(|p| p.bool(READ_ONLY_PREF));
    let answers: Vec<Value> = query
        .iter()
        .map(|path| {
            let lockable = locks.is_lockable(path);
            let lock = locks.lock_of(path);
            json!({
                "path": path,
                "lockable": lockable,
                "locked": lock.is_some(),
                "owner": lock.map(|l| l.owner.clone()),
                "mine": lock.is_some_and(|l| l.mine),
                "read_only": read_only && lockable && !lock.is_some_and(|l| l.mine),
            })
        })
        .collect();
    json!({
        "support": locks.support.name(),
        "reason": reason,
        "patterns": locks.rules.patterns(),
        "locks": locks.locks.iter().map(LockInfo::to_json).collect::<Vec<_>>(),
        "mine": locks.locks.iter().filter(|l| l.mine).map(|l| l.path.clone()).collect::<Vec<_>>(),
        "theirs": locks.locks.iter().filter(|l| !l.mine).map(|l| l.path.clone()).collect::<Vec<_>>(),
        "unlock_offer": locks.unlock_offer,
        "prompts": locks.prompts.iter().map(LockPrompt::to_json).collect::<Vec<_>>(),
        "lock_calls": locks.lock_calls,
        "unlock_calls": locks.unlock_calls,
        "query": answers,
    })
}

fn locks_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let query = if params.get("paths").is_some() {
        paths_of(params)
            .and_then(|paths| {
                paths
                    .iter()
                    .map(|p| super::git::clean_project_path(p))
                    .collect::<Result<Vec<_>, _>>()
            })
            .map_err(|e| rpc_error(&e))?
    } else {
        Vec::new()
    };
    if params.get("refresh").and_then(Value::as_bool) != Some(false) {
        refresh_now(world);
    }
    Ok(locks_json(world, &query))
}

fn lock_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let paths = paths_of(params).map_err(|e| rpc_error(&e))?;
    if world
        .get_resource::<VcsLocks>()
        .is_none_or(|l| l.support != LockSupport::Available)
    {
        refresh_now(world);
    }
    let taken = lock_paths(world, &paths).map_err(|e| rpc_error(&e))?;
    Ok(json!({ "locked": taken.iter().map(LockInfo::to_json).collect::<Vec<_>>() }))
}

fn unlock_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let unlocked = if params.get("all_mine").and_then(Value::as_bool) == Some(true) {
        if world
            .get_resource::<VcsLocks>()
            .is_none_or(|l| l.support != LockSupport::Available)
        {
            refresh_now(world);
        }
        unlock_all_mine(world)
    } else {
        let paths = paths_of(params).map_err(|e| rpc_error(&e))?;
        if world
            .get_resource::<VcsLocks>()
            .is_none_or(|l| l.support != LockSupport::Available)
        {
            refresh_now(world);
        }
        unlock_paths(world, &paths)
    }
    .map_err(|e| rpc_error(&e))?;
    Ok(json!({ "unlocked": unlocked }))
}

// The tests are `#![cfg(test)]` themselves (the coverage scans read that marker).
#[path = "locks_tests.rs"]
mod tests;
