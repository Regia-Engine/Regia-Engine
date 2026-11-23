// SPDX-License-Identifier: MPL-2.0
//! Git history for golden PNGs: which paths changed, and the commits that changed them.
//!
//! `git diff -z --name-status --find-renames` is the listing. Rename and copy records have two
//! paths; every other status has one. A path counts when any `goldens` directory segment holds
//! a `.png`. Commit messages come from `git log <from>..<to> -- <path>` (both sides of a
//! rename), kept only when the hash is in that revision range.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// How a path changed between the two trees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeKind {
    /// Present only at the newer revision.
    Added,
    /// Present at both revisions.
    Modified,
    /// Present only at the older revision.
    Deleted,
    /// Renamed (git similarity status `R`).
    Renamed,
    /// Copied (git similarity status `C`).
    Copied,
    /// File type change (`T`).
    TypeChange,
}

impl ChangeKind {
    /// Short label used on the sheet and in `index.md`.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::Copied => "copied",
            Self::TypeChange => "type change",
        }
    }
}

/// One path from `git diff --name-status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathChange {
    /// Kind of change.
    pub kind: ChangeKind,
    /// The path at the newer revision, or the only path for an add, edit, or delete.
    pub path: String,
    /// The path at the older revision when this is a rename or a copy.
    pub previous: Option<String>,
}

/// One commit that touched a golden.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitNote {
    /// Full commit hash.
    pub hash: String,
    /// First line of the message.
    pub subject: String,
    /// The rest of the message, trimmed. Empty when the commit is a one-line subject.
    pub body: String,
}

/// `true` when `path` is a PNG inside a directory named `goldens`.
pub(crate) fn is_golden_png(path: &str) -> bool {
    let path = path.replace('\\', "/");
    let Some((_, ext)) = path.rsplit_once('.') else {
        return false;
    };
    if !ext.eq_ignore_ascii_case("png") {
        return false;
    }
    path.split('/').any(|segment| segment == "goldens")
}

/// Parse `git diff -z --name-status` output.
///
/// Rename (`R###`) and copy (`C###`) records are status, old path, new path. Every other
/// status is status, path. A trailing NUL is optional.
pub(crate) fn parse_name_status_z(data: &[u8]) -> Result<Vec<PathChange>> {
    let parts: Vec<&[u8]> = data
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .collect();
    let mut index = 0;
    let mut out = Vec::new();
    while index < parts.len() {
        let status = std::str::from_utf8(parts[index]).context("git status code is not utf-8")?;
        index += 1;
        if status.starts_with('R') || status.starts_with('C') {
            if index + 1 >= parts.len() {
                bail!("git name-status rename `{status}` is missing a path");
            }
            let previous = utf8_path(parts[index])?;
            let path = utf8_path(parts[index + 1])?;
            index += 2;
            let kind = if status.starts_with('C') {
                ChangeKind::Copied
            } else {
                ChangeKind::Renamed
            };
            out.push(PathChange {
                kind,
                path,
                previous: Some(previous),
            });
        } else {
            if index >= parts.len() {
                bail!("git name-status `{status}` is missing a path");
            }
            let path = utf8_path(parts[index])?;
            index += 1;
            let kind = match status {
                "A" => ChangeKind::Added,
                "D" => ChangeKind::Deleted,
                "T" => ChangeKind::TypeChange,
                _ => ChangeKind::Modified,
            };
            out.push(PathChange {
                kind,
                path,
                previous: None,
            });
        }
    }
    Ok(out)
}

/// Keep golden PNGs (either side of a rename) and sort by the newer path.
pub(crate) fn select_goldens(changes: Vec<PathChange>) -> Vec<PathChange> {
    let mut kept: Vec<_> = changes
        .into_iter()
        .filter(|change| {
            is_golden_png(&change.path) || change.previous.as_ref().is_some_and(|path| is_golden_png(path))
        })
        .collect();
    kept.sort_by(|a, b| a.path.cmp(&b.path).then(a.previous.cmp(&b.previous)));
    kept
}

/// Parse `git log --format=%x1e%H%x1f%s%x1f%b`.
pub(crate) fn parse_commit_log(text: &str) -> Vec<CommitNote> {
    let mut notes = Vec::new();
    for record in text.split('\u{1e}') {
        if record.trim().is_empty() {
            continue;
        }
        let mut fields = record.splitn(3, '\u{1f}');
        let hash = fields.next().unwrap_or("").trim();
        if hash.is_empty() {
            continue;
        }
        let subject = fields.next().unwrap_or("").trim().to_string();
        let body = fields.next().unwrap_or("").trim().to_string();
        notes.push(CommitNote {
            hash: hash.to_string(),
            subject,
            body,
        });
    }
    notes
}

/// Resolve `rev` to a full commit hash.
pub(crate) fn resolve_commit(root: &Path, rev: &str) -> Result<String> {
    let spec = format!("{rev}^{{commit}}");
    let text = git_text(root, &["rev-parse", "--verify", &spec])?;
    let hash = text.trim();
    if hash.is_empty() {
        bail!("git rev-parse returned an empty hash for {rev}");
    }
    Ok(hash.to_string())
}

/// Golden PNG paths that differ between the two commits, older path then newer path.
pub(crate) fn changed_goldens(root: &Path, from: &str, to: &str) -> Result<Vec<PathChange>> {
    let bytes = git_ok(
        root,
        &["diff", "-z", "--name-status", "--find-renames", from, to, "--"],
    )?;
    Ok(select_goldens(parse_name_status_z(&bytes)?))
}

/// Commit hashes reachable from `to` but not from `from`, newest first.
pub(crate) fn commit_order(root: &Path, from: &str, to: &str) -> Result<HashMap<String, usize>> {
    let spec = format!("{from}..{to}");
    let text = git_text(root, &["rev-list", &spec])?;
    let mut order = HashMap::new();
    for (index, line) in text.lines().enumerate() {
        let hash = line.trim();
        if !hash.is_empty() {
            order.insert(hash.to_string(), index);
        }
    }
    Ok(order)
}

/// Commits in `from..to` that touch `paths`, newest first.
///
/// The bool is `true` when nothing in the range touched the path and the notes are the latest
/// commit at `to` instead (so the index still has a message).
pub(crate) fn file_commits(
    root: &Path,
    from: &str,
    to: &str,
    paths: &[&str],
    range: &HashMap<String, usize>,
) -> Result<(Vec<CommitNote>, bool)> {
    let spec = format!("{from}..{to}");
    let format = format!("--format={LOG_FORMAT}");
    let mut notes = Vec::new();
    for path in paths {
        let text = git_text(root, &["log", &format, &spec, "--", path])?;
        notes.extend(parse_commit_log(&text));
    }
    notes.retain(|note| range.contains_key(&note.hash));
    notes.sort_by_key(|note| range.get(&note.hash).copied().unwrap_or(usize::MAX));
    let mut seen = HashSet::new();
    notes.retain(|note| seen.insert(note.hash.clone()));
    if !notes.is_empty() {
        return Ok((notes, false));
    }
    let Some(path) = paths.first() else {
        return Ok((Vec::new(), false));
    };
    let text = git_text(root, &["log", "-1", &format, to, "--", path])?;
    let fallback = parse_commit_log(&text);
    if fallback.is_empty() {
        Ok((Vec::new(), false))
    } else {
        Ok((fallback, true))
    }
}

/// Blob bytes of `rev:path`, or `None` when that revision has no such path.
pub(crate) fn show_blob(root: &Path, rev: &str, path: &str) -> Result<Option<Vec<u8>>> {
    let spec = format!("{rev}:{path}");
    let output = git(root, &["show", &spec])?;
    if output.status.success() {
        return Ok(Some(output.stdout));
    }
    let err = String::from_utf8_lossy(&output.stderr);
    if err.contains("does not exist") || err.contains("exists on disk, but not") {
        return Ok(None);
    }
    bail!("git show {spec} failed: {}", err.trim());
}

const LOG_FORMAT: &str = "%x1e%H%x1f%s%x1f%b";

fn utf8_path(bytes: &[u8]) -> Result<String> {
    String::from_utf8(bytes.to_vec()).context("git path is not utf-8")
}

fn git(root: &Path, args: &[&str]) -> Result<std::process::Output> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root);
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        cmd.env_remove(key);
    }
    let output = cmd
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    Ok(output)
}

fn git_ok(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = git(root, args)?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn git_text(root: &Path, args: &[&str]) -> Result<String> {
    let bytes = git_ok(root, args)?;
    String::from_utf8(bytes).with_context(|| format!("git {} output is not utf-8", args.join(" ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_filter_is_the_goldens_directory() {
        assert!(is_golden_png("crates/regia_test_utils/goldens/surface_maps.png"));
        assert!(is_golden_png("samples/lookdev/goldens/wide_vista.png"));
        assert!(is_golden_png("goldens/nested/a.PNG"));
        assert!(!is_golden_png("crates/foo/goldens/readme.md"));
        assert!(!is_golden_png("crates/foo/not-goldens/a.png"));
        assert!(!is_golden_png("crates/goldens.png"));
        assert!(!is_golden_png("docs/evidence/foo.png"));
    }

    #[test]
    fn parses_nul_name_status_including_renames() {
        let raw = b"M\0crates/a/goldens/a.png\0\
            A\0crates/a/goldens/b.png\0\
            D\0crates/a/goldens/c.png\0\
            T\0crates/a/goldens/t.png\0\
            R090\0old/goldens/d.png\0new/goldens/e.png\0\
            C100\0old/goldens/f.png\0copy/goldens/g.png\0";
        let changes = parse_name_status_z(raw).expect("parse");
        assert_eq!(changes.len(), 6);
        assert_eq!(changes[0].kind, ChangeKind::Modified);
        assert_eq!(changes[1].kind, ChangeKind::Added);
        assert_eq!(changes[2].kind, ChangeKind::Deleted);
        assert_eq!(changes[3].kind, ChangeKind::TypeChange);
        assert_eq!(changes[4].kind, ChangeKind::Renamed);
        assert_eq!(changes[4].previous.as_deref(), Some("old/goldens/d.png"));
        assert_eq!(changes[4].path, "new/goldens/e.png");
        assert_eq!(changes[5].kind, ChangeKind::Copied);
        assert_eq!(changes[4].kind.label(), "renamed");
    }

    #[test]
    fn select_goldens_drops_other_pngs_and_sorts() {
        let changes = parse_name_status_z(
            b"M\0docs/evidence/z.png\0A\0b/goldens/a.png\0M\0a/goldens/m.png\0D\0notes/picture.png\0",
        )
        .expect("parse");
        let kept = select_goldens(changes);
        let paths: Vec<_> = kept.iter().map(|change| change.path.as_str()).collect();
        assert_eq!(paths, vec!["a/goldens/m.png", "b/goldens/a.png"]);
    }

    #[test]
    fn a_rename_out_of_goldens_is_kept() {
        let changes =
            parse_name_status_z(b"R100\0demo/goldens/old.png\0demo/elsewhere/old.png\0").expect("parse");
        let kept = select_goldens(changes);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].path, "demo/elsewhere/old.png");
    }

    #[test]
    fn parses_commit_records() {
        let text = "\u{1e}abc\u{1f}subject one\u{1f}body line\nmore\u{1e}def\u{1f}subject two\u{1f}\n";
        let notes = parse_commit_log(text);
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].hash, "abc");
        assert_eq!(notes[0].subject, "subject one");
        assert_eq!(notes[0].body, "body line\nmore");
        assert_eq!(notes[1].hash, "def");
        assert_eq!(notes[1].subject, "subject two");
        assert_eq!(notes[1].body, "");
    }

    #[test]
    fn truncated_rename_is_an_error() {
        let err = parse_name_status_z(b"R100\0only-one").expect_err("truncated");
        let message = format!("{err:#}");
        assert!(message.contains("missing a path"), "{message}");
    }
}
