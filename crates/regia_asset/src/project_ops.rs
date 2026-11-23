// SPDX-License-Identifier: MPL-2.0
//! Move/rename a project asset and keep references valid (spec 12 § 9, M11).
//!
//! The content browser's move/rename moves the source file *and* its `.meta` sidecar, so the
//! [`MetaSidecar::id`] never changes: references that store `asset:<guid>` keep working with no
//! edit. References that store the source path (a scene, prefab, level or material file names
//! `assets/<path>` or `<path>` in a string) are rewritten to the new path.
//!
//! Two match rules, both treating `/` as part of a path (ADR-5877):
//!
//! - **Rewrite** ([`rewrite_path_references`]) edits string literals only. A string whose whole
//!   text, after an optional `assets/` prefix, is the moved path, or starts with the moved path
//!   and `/` (a file under a moved folder), is rewritten. Comments, field names and every other
//!   string stay as written, so `props/rock.glb` is not a mention of `rock.glb` and a field named
//!   `props` survives moving the `props` folder. A string that is only a bare top-level name
//!   with no extension (`"props"`) is treated as a tag, not a path: `"assets/props"` is rewritten.
//! - **Referencer scan** ([`find_referencers`], also the delete check) is wider: any whole-path
//!   mention in the text counts, comments included, so a delete stays conservative. A mention
//!   must not continue a longer path on either side (`redrock.prefab.ron`, `props/rock.glb` and
//!   `rock.glb.meta` do not mention `rock.glb`); `assets/` may precede it.
//!
//! [`move_asset`] is all-or-nothing: every check runs before the first change, and when a write
//! or a rename fails the referencer rewrites, the source rename and any created folders are
//! rolled back. Undo is the same operation with `from` and `to` swapped.

use std::path::{Path, PathBuf};

use crate::meta::{MetaSidecar, meta_path_for};

/// What a successful [`move_asset`] changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveOutcome {
    /// Project-relative slash paths (`assets/…`) of the referencer files whose text was rewritten.
    pub fixed_referencers: Vec<String>,
}

/// Why a move/rename was refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum MoveError {
    /// The source path is not on disk.
    #[error("source does not exist: {0}")]
    SourceMissing(String),
    /// The destination path, or the place its `.meta` sidecar would go, is already taken
    /// (nothing was changed).
    #[error("destination already exists: {0}")]
    DestinationExists(String),
    /// The path is empty, absolute, climbs out of `assets/`, or moves a folder into itself
    /// (nothing was changed).
    #[error("invalid asset path: {0}")]
    InvalidPath(String),
    /// A filesystem operation failed. The move was rolled back; `message` says so, or names what
    /// the rollback could not restore.
    #[error("cannot move {from} to {to}: {message}")]
    Io {
        /// Source, project-relative.
        from: String,
        /// Destination, project-relative.
        to: String,
        /// The OS error message, plus the rollback outcome.
        message: String,
    },
}

/// Normalizes an editor-style project-relative path (`assets/props/rock.glb` or `props/rock.glb`)
/// to the `assets/`-relative form used on disk, rejecting paths that leave `assets/`.
fn normalize_rel(rel: &str) -> Result<String, MoveError> {
    let rel = rel.trim();
    if Path::new(rel).is_absolute() || rel.starts_with('/') {
        return Err(MoveError::InvalidPath(rel.to_string()));
    }
    let rel = strip_assets(rel);
    if rel.is_empty()
        || rel.contains('\\')
        || rel
            .split('/')
            .any(|part| part == ".." || part.is_empty() || part == ".")
    {
        return Err(MoveError::InvalidPath(rel.to_string()));
    }
    Ok(rel.to_string())
}

/// `rel` without one leading `assets/`.
fn strip_assets(rel: &str) -> &str {
    rel.strip_prefix("assets/").unwrap_or(rel)
}

/// The project-relative path (`assets/<p>`) that [`move_asset`] acts on for an editor-style
/// path `rel` (the `assets/` prefix is optional). Callers that confine request paths check this
/// form, not the raw parameter.
///
/// # Errors
/// [`MoveError::InvalidPath`] when `rel` is empty, absolute or climbs out of `assets/`.
pub fn asset_project_path(rel: &str) -> Result<String, MoveError> {
    normalize_rel(rel).map(|rel| format!("assets/{rel}"))
}

/// A project-relative slash path as the editor reports it (`assets/props/rock.glb`).
fn slash(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text.into_owned()
    }
}

/// Every `.ron` file under `<project_root>/assets` that the referencer scan reads: hidden entries
/// and `.meta` sidecars are skipped, and symlinks are not followed (a link could lead out of the
/// project or into a cycle).
fn ron_files(project_root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![project_root.join("assets")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() && name.ends_with(".ron") && !name.ends_with(".meta") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Project files that mention `rel` (a cheap referencer check for moves/deletes: scene, prefab,
/// level and material files are all text). A file matches by path or by the asset id in `rel`'s
/// `.meta` sidecar (saved instances store `asset:<guid>`, not the path). Matches use the
/// referencer-scan rule of the module docs; symlinks are not followed. Returned paths are
/// project-relative with `/` separators, sorted.
#[must_use]
pub fn find_referencers(project_root: &Path, rel: &str) -> Vec<String> {
    referencers(project_root, rel, true)
}

/// [`find_referencers`] by path mention only: the files an `asset:<guid>` reference alone would
/// not list. A move keeps the id (the `.meta` moves with the file), so only these name a path
/// that a move changes.
#[must_use]
pub fn find_path_referencers(project_root: &Path, rel: &str) -> Vec<String> {
    referencers(project_root, rel, false)
}

/// The referencer scan behind [`find_referencers`] (`by_id`: also match the `.meta` asset id).
fn referencers(project_root: &Path, rel: &str, by_id: bool) -> Vec<String> {
    let needle = strip_assets(rel.trim()).to_string();
    let guid = by_id
        .then(|| std::fs::read(meta_path_for(&project_root.join("assets").join(&needle))).ok())
        .flatten()
        .and_then(|bytes| MetaSidecar::from_ron(&bytes).ok())
        .map(|meta| meta.id.to_string());
    let mut found = Vec::new();
    for path in ron_files(project_root) {
        if let Ok(text) = std::fs::read_to_string(&path)
            && (mentions_path(&text, &needle)
                || guid.as_ref().is_some_and(|guid| text.contains(guid.as_str())))
            && let Ok(strip) = path.strip_prefix(project_root)
        {
            found.push(slash(strip));
        }
    }
    found.sort();
    found
}

/// Whether `c` continues a path or a file name (`/` included: `props/rock.glb` is a different
/// path from `rock.glb`).
fn is_path_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '\\')
}

/// A bare top-level name with no extension (`props`): as a whole string or a bare word it reads
/// as a tag or a field name, so only `assets/<name>` or `<name>/…` count as its path.
fn is_bare_folder_name(needle: &str) -> bool {
    !needle.contains('/') && !needle.contains('.')
}

/// Whether `text` mentions `needle` as a whole path: the characters around a match must not
/// continue a longer path (an `assets/` prefix is allowed before it), and a bare folder name
/// counts only with the prefix or a `/` after it.
fn mentions_path(text: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let bare_folder = is_bare_folder_name(needle);
    text.match_indices(needle).any(|(at, _)| {
        let before = &text[..at];
        let prefixed = before
            .strip_suffix("assets/")
            .is_some_and(|head| head.chars().next_back().is_none_or(|c| !is_path_char(c)));
        let left = prefixed || before.chars().next_back().is_none_or(|c| !is_path_char(c));
        let next = text[at + needle.len()..].chars().next();
        let right = match next {
            Some('/') => true,
            Some(c) => !is_path_char(c) && (prefixed || !bare_folder),
            None => prefixed || !bare_folder,
        };
        left && right
    })
}

/// Byte ranges of the contents of the string literals in RON `text` (normal and raw strings),
/// skipping comments and char literals so a quote inside them does not start a string. A
/// normal string's range is its raw source text (escapes not decoded).
fn string_literal_ranges(text: &str) -> Vec<(usize, usize, bool)> {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < len {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < len && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                // RON block comments nest.
                let mut depth = 0usize;
                while i < len {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            b'"' => {
                let start = i + 1;
                let mut j = start;
                while j < len && bytes[j] != b'"' {
                    j += if bytes[j] == b'\\' { 2 } else { 1 };
                }
                let end = j.min(len);
                ranges.push((start, end, false));
                i = end + 1;
            }
            b'r' if (i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_'))
                && matches!(bytes.get(i + 1), Some(b'"' | b'#')) =>
            {
                let mut j = i + 1;
                let mut hashes = 0;
                while j < len && bytes[j] == b'#' {
                    hashes += 1;
                    j += 1;
                }
                if bytes.get(j) != Some(&b'"') {
                    i += 1;
                    continue;
                }
                let start = j + 1;
                let mut k = start;
                let end = loop {
                    if k >= len {
                        break len;
                    }
                    if bytes[k] == b'"'
                        && bytes[k + 1..].iter().take(hashes).filter(|&&b| b == b'#').count() == hashes
                    {
                        break k;
                    }
                    k += 1;
                };
                ranges.push((start, end, true));
                i = (end + 1 + hashes).min(len);
            }
            b'\'' => {
                // A char literal: `'x'`, `'"'` or `'\''`. Anything else is a stray apostrophe.
                let j = i + 1;
                let after = if bytes.get(j) == Some(&b'\\') {
                    j + 2
                } else {
                    j + text[j..].chars().next().map_or(1, char::len_utf8)
                };
                i = if bytes.get(after) == Some(&b'\'') {
                    after + 1
                } else {
                    i + 1
                };
            }
            _ => i += 1,
        }
    }
    ranges
}

/// The rewritten string contents when `content` is a path mention of `from` (see the module
/// docs), else `None`.
fn rewrite_string(content: &str, from: &str, to: &str) -> Option<String> {
    let (prefix, rest) = match content.strip_prefix("assets/") {
        Some(rest) => ("assets/", rest),
        None => ("", content),
    };
    if rest == from {
        if prefix.is_empty() && is_bare_folder_name(from) {
            return None;
        }
        return Some(format!("{prefix}{to}"));
    }
    let below = rest.strip_prefix(from)?.strip_prefix('/')?;
    Some(format!("{prefix}{to}/{below}"))
}

/// Rewrites path mentions of `from_rel` in RON `text` to `to_rel` (both editor-style
/// project-relative paths; the `assets/` prefix is optional). Only string literals are edited,
/// with the rewrite rule of the module docs. Returns `None` when nothing changed.
#[must_use]
pub fn rewrite_path_references(text: &str, from_rel: &str, to_rel: &str) -> Option<String> {
    let from = strip_assets(from_rel.trim());
    let to = strip_assets(to_rel.trim());
    if from == to || from.is_empty() || to.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut changed = false;
    for (start, end, raw) in string_literal_ranges(text) {
        let content = &text[start..end];
        // An escape in a normal string means it is not a plain path; leave it.
        if !raw && content.contains('\\') {
            continue;
        }
        if let Some(new) = rewrite_string(content, from, to) {
            out.push_str(&text[copied..start]);
            out.push_str(&new);
            copied = end;
            changed = true;
        }
    }
    if !changed {
        return None;
    }
    out.push_str(&text[copied..]);
    Some(out)
}

/// Whether `to` names the same directory entry as `from` under another letter case (a
/// case-only rename on a case-insensitive file system): same parent, names equal ignoring case,
/// and the parent's listing holds `from`'s name but not `to`'s.
fn same_entry_other_case(from: &Path, to: &Path) -> bool {
    let (Some(from_parent), Some(to_parent)) = (from.parent(), to.parent()) else {
        return false;
    };
    let (Some(from_name), Some(to_name)) = (from.file_name(), to.file_name()) else {
        return false;
    };
    if from_parent != to_parent
        || from_name == to_name
        || from_name.to_string_lossy().to_lowercase() != to_name.to_string_lossy().to_lowercase()
    {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(from_parent) else {
        return false;
    };
    let names: Vec<std::ffi::OsString> = entries.filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
    names.iter().any(|n| n == from_name) && !names.iter().any(|n| n == to_name)
}

/// Whether something is at `to` other than `from` itself under another letter case.
fn taken(from: &Path, to: &Path) -> std::io::Result<bool> {
    match to.symlink_metadata() {
        Ok(_) => Ok(!same_entry_other_case(from, to)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Renames `from` to `to`; a case-only rename goes through a temporary sibling name, which works
/// on case-insensitive file systems that would otherwise treat it as a no-op.
fn rename(from: &Path, to: &Path, case_only: bool) -> std::io::Result<()> {
    if !case_only {
        return std::fs::rename(from, to);
    }
    let name = from
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = from.with_file_name(format!(".{name}.regia-move-{}.tmp", std::process::id()));
    std::fs::rename(from, &tmp)?;
    std::fs::rename(&tmp, to).inspect_err(|_| {
        let _ = std::fs::rename(&tmp, from);
    })
}

/// A referencer rewrite of [`move_asset`]: the file, its original text and the new text.
struct Rewrite {
    full: PathBuf,
    rel: String,
    original: String,
    rewritten: String,
}

/// What [`move_asset`] has changed so far, so a failure can put it back.
#[derive(Default)]
struct Applied {
    /// Folders created for the destination, deepest first.
    created_dirs: Vec<PathBuf>,
    /// Referencers already rewritten (their originals are in the plan).
    rewritten: usize,
    /// The source moved: (to, from, case_only).
    source: Option<(PathBuf, PathBuf, bool)>,
}

impl Applied {
    /// Undoes everything, newest first; returns what could not be restored.
    fn roll_back(self, rewrites: &[Rewrite]) -> Vec<String> {
        let mut failed = Vec::new();
        if let Some((to, from, case_only)) = &self.source
            && let Err(e) = rename(to, from, *case_only)
        {
            failed.push(format!("{} stays at {}: {e}", from.display(), to.display()));
        }
        for rewrite in rewrites.iter().take(self.rewritten) {
            if let Err(e) = write_atomic(&rewrite.full, rewrite.original.as_bytes()) {
                failed.push(format!("{} keeps the new path: {e}", rewrite.rel));
            }
        }
        for dir in &self.created_dirs {
            let _ = std::fs::remove_dir(dir);
        }
        failed
    }
}

/// Moves (or renames) `from_rel` to `to_rel` inside the project's `assets/` folder, keeping every
/// reference valid (spec 12 § 9, M11).
///
/// The source keeps its [`MetaSidecar::id`]: the `.meta` sidecar moves with it, so `asset:<guid>`
/// references need no edit, and string literals that name the old path are rewritten to the new
/// one (rule in the module docs). `from_rel`/`to_rel` are editor-style project-relative paths
/// (the `assets/` prefix is optional; a directory moves with everything under it; a case-only
/// rename works on case-insensitive file systems). Symlinked folders are not scanned.
///
/// All-or-nothing: every refusal comes before the first change, and a failed write or rename
/// rolls back the referencer rewrites, the source rename and the folders it created.
///
/// # Errors
///
/// [`MoveError::SourceMissing`] when the source is not on disk,
/// [`MoveError::DestinationExists`] when the destination or its `.meta` place is taken,
/// [`MoveError::InvalidPath`] for a path outside `assets/` or a folder moved into itself,
/// [`MoveError::Io`] when a filesystem step fails (rolled back; the message names anything the
/// rollback could not restore).
pub fn move_asset(project_root: &Path, from_rel: &str, to_rel: &str) -> Result<MoveOutcome, MoveError> {
    let from = normalize_rel(from_rel)?;
    let to = normalize_rel(to_rel)?;
    if to.starts_with(&format!("{from}/")) {
        return Err(MoveError::InvalidPath(format!(
            "assets/{to} is inside assets/{from}"
        )));
    }
    let assets = project_root.join("assets");
    let from_full = assets.join(&from);
    let to_full = assets.join(&to);
    let io = |message: String| MoveError::Io {
        from: format!("assets/{from}"),
        to: format!("assets/{to}"),
        message,
    };
    match from_full.symlink_metadata() {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(MoveError::SourceMissing(format!("assets/{from}")));
        }
        Err(e) => return Err(io(e.to_string())),
    }
    if from == to || taken(&from_full, &to_full).map_err(|e| io(e.to_string()))? {
        return Err(MoveError::DestinationExists(format!("assets/{to}")));
    }
    let case_only = same_entry_other_case(&from_full, &to_full);
    let from_meta = meta_path_for(&from_full);
    let to_meta = meta_path_for(&to_full);
    let has_meta = from_meta.symlink_metadata().is_ok();
    // A stale sidecar at the destination would pair with the moved file (and a rename onto a
    // folder fails after the source moved), so a taken `.meta` place refuses the move.
    if taken(&from_meta, &to_meta).map_err(|e| io(e.to_string()))? {
        return Err(MoveError::DestinationExists(format!("assets/{to}.meta")));
    }

    // Plan the referencer rewrites in memory first. The file itself can "reference" its own
    // path (a material naming its texture); skip it.
    let mut rewrites = Vec::new();
    for full in ron_files(project_root) {
        if full == from_full {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&full) else {
            continue;
        };
        let Some(rewritten) = rewrite_path_references(&text, &from, &to) else {
            continue;
        };
        let rel = full.strip_prefix(project_root).map(slash).unwrap_or_default();
        rewrites.push(Rewrite {
            full,
            rel,
            original: text,
            rewritten,
        });
    }

    let mut applied = Applied::default();
    let fail = |applied: Applied, rewrites: &[Rewrite], what: String| {
        let failed = applied.roll_back(rewrites);
        let message = if failed.is_empty() {
            format!("{what} (rolled back; nothing changed)")
        } else {
            format!("{what}; rollback incomplete: {}", failed.join("; "))
        };
        io(message)
    };

    if let Some(parent) = to_full.parent() {
        let mut missing = Vec::new();
        let mut probe = parent;
        while probe.symlink_metadata().is_err() && probe != assets {
            missing.push(probe.to_path_buf());
            match probe.parent() {
                Some(up) => probe = up,
                None => break,
            }
        }
        applied.created_dirs = missing;
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Err(fail(
                applied,
                &rewrites,
                format!("cannot create {}: {e}", parent.display()),
            ));
        }
    }
    // Rewrite references before the rename so a referencer never points at a missing file.
    for rewrite in &rewrites {
        if let Err(e) = write_atomic(&rewrite.full, rewrite.rewritten.as_bytes()) {
            return Err(fail(applied, &rewrites, e));
        }
        applied.rewritten += 1;
    }
    if let Err(e) = rename(&from_full, &to_full, case_only) {
        return Err(fail(applied, &rewrites, e.to_string()));
    }
    applied.source = Some((to_full.clone(), from_full.clone(), case_only));
    if has_meta && let Err(e) = rename(&from_meta, &to_meta, case_only) {
        return Err(fail(
            applied,
            &rewrites,
            format!("cannot move the .meta sidecar: {e}"),
        ));
    }
    let mut fixed_referencers: Vec<String> = rewrites.into_iter().map(|r| r.rel).collect();
    fixed_referencers.sort();
    Ok(MoveOutcome { fixed_referencers })
}

/// Writes `bytes` to `path` atomically (temp file + rename in the same directory).
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("new.tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot rename to {}: {e}", path.display()))
}
