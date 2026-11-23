// SPDX-License-Identifier: MPL-2.0
//! Validation before commit (`docs/specs/32-asset-validation-and-audit.md` § 4, ADR-7789): the
//! files staged in git, a check of what is staged, and the pre-commit hook that runs it.
//!
//! [`check_staged`] lists the index's changes against `HEAD` (`git diff --cached`), keeps the files
//! under the project directory and reads every staged text asset **from the index** (what the commit
//! will hold, not the working tree): RON assets and `.meta` sidecars must parse as RON, `.gltf` and
//! `.json` as JSON. A file that does not parse is an error finding (`staged_parse_error`). The
//! staged paths, project-relative as the registry keeps them and a `.meta` mapped to its asset, are
//! [`StagedCheck::changed_assets`]: a caller with a cooked registry also runs the registry
//! validators over them (`Scope::Changed`), which `regia assets validate --staged` does.
//!
//! [`install_hook`] writes `.git/hooks/pre-commit` (wherever `git rev-parse --git-path` puts it, so
//! `core.hooksPath` and linked worktrees work): a POSIX `sh` script, marked with [`HOOK_MARKER`],
//! that runs `regia assets validate --staged`. It never overwrites a hook it did not write unless
//! asked to (the old hook is kept beside it and [`uninstall_hook`] puts it back).
//!
//! Git runs as a program with an argument vector, never through a shell. Inside a hook git exports
//! `GIT_DIR` and `GIT_INDEX_FILE`, possibly relative to the hook's working directory; they are made
//! absolute before git runs in another directory, so `git commit -a` (a temporary index) is checked
//! as committed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::validators::{Finding, Severity};

/// The line that marks a hook [`install_hook`] wrote.
pub const HOOK_MARKER: &str = "regia-pre-commit-hook";

/// Where [`install_hook`] keeps a hook it replaced with `force` (beside the hook).
pub const HOOK_BACKUP_SUFFIX: &str = ".before-regia";

/// Largest staged file read for the syntax check, bytes; a larger one is reported, not read.
pub const MAX_STAGED_BYTES: u64 = 64 * 1024 * 1024;

/// The finding code of a staged file that does not parse.
pub const CODE_PARSE_ERROR: &str = "staged_parse_error";
/// The finding code of a staged file git could not read from the index.
pub const CODE_UNREADABLE: &str = "staged_unreadable";
/// The finding code of a staged file over [`MAX_STAGED_BYTES`] (info: not checked).
pub const CODE_TOO_LARGE: &str = "staged_too_large";

/// Why staged validation or a hook action could not run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PrecommitError {
    /// The `git` program did not start.
    #[error("git did not run: {0}")]
    GitMissing(String),
    /// The directory is not inside a git work tree.
    #[error("{0} is not inside a git work tree")]
    NotARepo(PathBuf),
    /// A git command failed.
    #[error("git {args} failed: {stderr}")]
    Git {
        /// The arguments.
        args: String,
        /// What git printed on stderr.
        stderr: String,
    },
    /// Reading or writing the hook file failed.
    #[error("{path}: {message}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The error.
        message: String,
    },
    /// The hook file exists and Regia did not write it.
    #[error("{0} exists and was not written by Regia (pass --force to replace it; it is kept beside it)")]
    ForeignHook(PathBuf),
    /// There is no pre-commit hook to remove.
    #[error("there is no pre-commit hook at {0}")]
    NoHook(PathBuf),
}

impl PrecommitError {
    /// The stable error code (lowercase ascii, digits and `_`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::GitMissing(_) => "git_not_found",
            Self::NotARepo(_) => "not_a_git_repo",
            Self::Git { .. } => "git_failed",
            Self::Io { .. } => "hook_io",
            Self::ForeignHook(_) => "hook_foreign",
            Self::NoHook(_) => "hook_missing",
        }
    }
}

/// How a file changed in the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StagedStatus {
    /// New in the index.
    Added,
    /// Changed (content, type or mode).
    Modified,
    /// Removed from the index.
    Deleted,
}

/// One staged file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StagedFile {
    /// The path relative to the repository root, `/` separators.
    pub path: String,
    /// The path relative to the project directory (`None` outside it).
    pub project_path: Option<String>,
    /// How it changed.
    pub status: StagedStatus,
}

/// A finding on one staged file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StagedFinding {
    /// The project-relative path.
    pub path: String,
    /// What is wrong.
    pub finding: Finding,
}

/// The outcome of [`check_staged`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StagedCheck {
    /// The repository's work tree root.
    pub repo_root: PathBuf,
    /// Every staged change under the project, sorted by path.
    pub files: Vec<StagedFile>,
    /// Files whose staged content was checked (parsed).
    pub checked: usize,
    /// The findings, sorted by path.
    pub findings: Vec<StagedFinding>,
    /// The registry paths the staged changes touch (project-relative, a `.meta` mapped to its
    /// asset), for a registry run with `Scope::Changed`.
    pub changed_assets: Vec<String>,
}

impl StagedCheck {
    /// Findings of `severity`.
    #[must_use]
    pub fn count(&self, severity: Severity) -> usize {
        self.findings
            .iter()
            .filter(|f| f.finding.severity == severity)
            .count()
    }

    /// Whether the check fails a commit: an error, or with `strict` a warning.
    #[must_use]
    pub fn failed(&self, strict: bool) -> bool {
        self.count(Severity::Error) > 0 || (strict && self.count(Severity::Warning) > 0)
    }
}

/// A `git` command with an argument vector, run in `dir`, with git's repository variables made
/// absolute against this process's working directory.
fn git(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(dir);
    let cwd = std::env::current_dir().ok();
    for name in ["GIT_DIR", "GIT_INDEX_FILE", "GIT_WORK_TREE", "GIT_COMMON_DIR"] {
        if let Some(value) = std::env::var_os(name) {
            let path = PathBuf::from(&value);
            if path.is_relative()
                && let Some(cwd) = &cwd
            {
                command.env(name, cwd.join(path));
            }
        }
    }
    command
}

/// Runs git in `dir` and answers its stdout.
fn run_git(dir: &Path, args: &[&str]) -> Result<Vec<u8>, PrecommitError> {
    let output = git(dir)
        .args(args)
        .output()
        .map_err(|e| PrecommitError::GitMissing(e.to_string()))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(PrecommitError::Git {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

/// The work tree root of the repository that holds `dir`.
///
/// # Errors
///
/// [`PrecommitError::NotARepo`] outside a work tree; [`PrecommitError::GitMissing`] without git.
pub fn repo_root(dir: &Path) -> Result<PathBuf, PrecommitError> {
    match run_git(dir, &["rev-parse", "--show-toplevel"]) {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out).trim().to_owned();
            if text.is_empty() {
                Err(PrecommitError::NotARepo(dir.to_path_buf()))
            } else {
                Ok(PathBuf::from(text))
            }
        }
        Err(PrecommitError::Git { .. }) => Err(PrecommitError::NotARepo(dir.to_path_buf())),
        Err(other) => Err(other),
    }
}

/// `path` relative to `base` with `/` separators, when it is under it (both made canonical when
/// they exist).
fn relative_to(path: &Path, base: &Path) -> Option<String> {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let base = std::fs::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());
    let rel = path.strip_prefix(&base).ok()?;
    let text = rel.to_string_lossy().replace('\\', "/");
    Some(if text.is_empty() { ".".to_owned() } else { text })
}

/// Parses `git diff --cached --name-status -z --no-renames`: status, NUL, path, NUL, ...
#[must_use]
pub fn parse_name_status(out: &[u8]) -> Vec<(StagedStatus, String)> {
    let mut fields = out.split(|b| *b == 0).filter(|f| !f.is_empty());
    let mut rows = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        let status = match status.first() {
            Some(b'A') => StagedStatus::Added,
            Some(b'D') => StagedStatus::Deleted,
            _ => StagedStatus::Modified,
        };
        rows.push((status, String::from_utf8_lossy(path).into_owned()));
    }
    rows
}

/// How a staged file's syntax is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Syntax {
    Ron,
    Json,
}

fn syntax_of(path: &str) -> Option<Syntax> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".ron") || lower.ends_with(".meta") {
        Some(Syntax::Ron)
    } else if lower.ends_with(".gltf") || lower.ends_with(".json") {
        Some(Syntax::Json)
    } else {
        None
    }
}

/// The parse error of `bytes` read as `syntax`, if any.
fn parse_error(bytes: &[u8], syntax: Syntax) -> Option<String> {
    match syntax {
        Syntax::Json => serde_json::from_slice::<serde_json::Value>(bytes)
            .err()
            .map(|e| e.to_string()),
        Syntax::Ron => match std::str::from_utf8(bytes) {
            Err(e) => Some(format!("not UTF-8: {e}")),
            Ok(text) => ron::from_str::<ron::Value>(text).err().map(|e| e.to_string()),
        },
    }
}

/// The registry path a project-relative path names (the registry keeps `assets/...`): a `.meta`
/// sidecar names its asset.
fn asset_path(project_path: &str) -> String {
    project_path
        .strip_suffix(".meta")
        .unwrap_or(project_path)
        .to_owned()
}

/// Checks what is staged under `project` (see the module docs).
///
/// # Errors
///
/// No git, `project` outside a work tree, or a failing `git diff --cached`.
pub fn check_staged(project: &Path) -> Result<StagedCheck, PrecommitError> {
    let _span = tracing::info_span!("check_staged").entered();
    let root = repo_root(project)?;
    let project_rel =
        relative_to(project, &root).ok_or_else(|| PrecommitError::NotARepo(project.to_path_buf()))?;
    let prefix = if project_rel == "." {
        String::new()
    } else {
        format!("{project_rel}/")
    };
    let out = run_git(
        &root,
        &["diff", "--cached", "--name-status", "-z", "--no-renames"],
    )?;
    let mut files: Vec<StagedFile> = parse_name_status(&out)
        .into_iter()
        .map(|(status, path)| StagedFile {
            project_path: path.strip_prefix(prefix.as_str()).map(str::to_owned),
            path,
            status,
        })
        .filter(|file| file.project_path.is_some())
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let mut findings = Vec::new();
    let mut checked = 0;
    let mut changed = BTreeSet::new();
    for file in &files {
        let Some(project_path) = &file.project_path else {
            continue;
        };
        changed.insert(asset_path(project_path));
        if file.status == StagedStatus::Deleted {
            continue;
        }
        let Some(syntax) = syntax_of(project_path) else {
            continue;
        };
        let spec = format!(":{}", file.path);
        let finding = match run_git(&root, &["cat-file", "-s", &spec]) {
            Err(e) => Some(Finding::new(CODE_UNREADABLE, Severity::Error, e.to_string())),
            Ok(size) => {
                let size: u64 = String::from_utf8_lossy(&size).trim().parse().unwrap_or(u64::MAX);
                if size > MAX_STAGED_BYTES {
                    Some(Finding::new(
                        CODE_TOO_LARGE,
                        Severity::Info,
                        format!("{size} bytes staged; files over {MAX_STAGED_BYTES} bytes are not parsed"),
                    ))
                } else {
                    match run_git(&root, &["cat-file", "blob", &spec]) {
                        Err(e) => Some(Finding::new(CODE_UNREADABLE, Severity::Error, e.to_string())),
                        Ok(bytes) => {
                            checked += 1;
                            parse_error(&bytes, syntax).map(|e| {
                                let mut finding = Finding::new(
                                    CODE_PARSE_ERROR,
                                    Severity::Error,
                                    format!("the staged file does not parse: {e}"),
                                );
                                finding.hint = Some(
                                    "fix the file and stage it again (git add), or commit with --no-verify"
                                        .to_owned(),
                                );
                                finding
                            })
                        }
                    }
                }
            }
        };
        if let Some(finding) = finding {
            findings.push(StagedFinding {
                path: project_path.clone(),
                finding,
            });
        }
    }
    Ok(StagedCheck {
        repo_root: root,
        files,
        checked,
        findings,
        changed_assets: changed.into_iter().collect(),
    })
}

// ---------------------------------------------------------------------------------------------
// The pre-commit hook
// ---------------------------------------------------------------------------------------------

/// The state of a project's pre-commit hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookState {
    /// Where git looks for the hook.
    pub path: PathBuf,
    /// A hook file exists there.
    pub exists: bool,
    /// It is Regia's (it carries [`HOOK_MARKER`]).
    pub regia: bool,
    /// A hook Regia replaced is kept beside it.
    pub backup: bool,
}

/// What [`install_hook`] or [`uninstall_hook`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookChange {
    /// The hook file.
    pub path: PathBuf,
    /// `installed`, `updated` (a Regia hook was rewritten), `replaced` (another hook was moved
    /// aside), `removed` or `restored` (the hook Regia replaced is back).
    pub action: &'static str,
    /// Where the replaced hook was kept, when one was.
    pub backup: Option<PathBuf>,
}

/// Where git runs the pre-commit hook for the repository holding `project`.
///
/// # Errors
///
/// As [`repo_root`].
pub fn hook_path(project: &Path) -> Result<PathBuf, PrecommitError> {
    let root = repo_root(project)?;
    let out = run_git(&root, &["rev-parse", "--git-path", "hooks/pre-commit"])?;
    let text = String::from_utf8_lossy(&out).trim().to_owned();
    let path = PathBuf::from(text);
    Ok(if path.is_relative() { root.join(path) } else { path })
}

fn backup_path(hook: &Path) -> PathBuf {
    let mut name = hook.as_os_str().to_owned();
    name.push(HOOK_BACKUP_SUFFIX);
    PathBuf::from(name)
}

fn io_error(path: &Path, e: &std::io::Error) -> PrecommitError {
    PrecommitError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

/// The state of the pre-commit hook of `project`'s repository.
///
/// # Errors
///
/// As [`repo_root`].
pub fn hook_state(project: &Path) -> Result<HookState, PrecommitError> {
    let path = hook_path(project)?;
    let text = std::fs::read_to_string(&path).ok();
    Ok(HookState {
        // The entry itself, not what it points at: a symlink to a missing script is a hook that is
        // there (and not Regia's), never a free slot to write over (review sprint-w12-rev-g5 #16).
        exists: std::fs::symlink_metadata(&path).is_ok(),
        regia: text.as_deref().is_some_and(|t| t.contains(HOOK_MARKER)),
        backup: backup_path(&path).exists(),
        path,
    })
}

/// `text` as one single-quoted `sh` word.
fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The hook script: runs `regia` (the given program when it exists, else `regia` on `PATH`) with
/// `assets validate --staged` on the project at `project_rel` (relative to the work tree root, where
/// git runs hooks).
#[must_use]
pub fn hook_script(regia: &Path, project_rel: &str) -> String {
    let program = regia.to_string_lossy().replace('\\', "/");
    format!(
        "#!/bin/sh\n\
         # {HOOK_MARKER} 1: written by `regia hooks install`; `regia hooks uninstall` removes it.\n\
         # Validates the staged assets before each commit; `git commit --no-verify` skips it once.\n\
         REGIA={program}\n\
         if [ ! -x \"$REGIA\" ]; then REGIA=regia; fi\n\
         exec \"$REGIA\" assets validate --staged --project {project}\n",
        program = sh_quote(&program),
        project = sh_quote(project_rel),
    )
}

/// Writes `text` to the temporary file beside the hook `path`, executable on Unix; returns that
/// file (it moves to `path` once everything else is in place).
fn write_temp(path: &Path, text: &str) -> Result<PathBuf, PrecommitError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io_error(dir, &e))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".regia-tmp");
    let tmp = PathBuf::from(tmp);
    let written = std::fs::write(&tmp, text).map_err(|e| io_error(&tmp, &e));
    #[cfg(unix)]
    let written = written.and_then(|()| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).map_err(|e| io_error(&tmp, &e))
    });
    if let Err(err) = written {
        // A half-written file is ours to remove; anything else there is not.
        if tmp.is_file() {
            let _ = std::fs::remove_file(&tmp);
        }
        return Err(err);
    }
    Ok(tmp)
}

/// Where a foreign hook is moved aside: [`HOOK_BACKUP_SUFFIX`], or the first free numbered name
/// after it (`.before-regia.2`, ...), so an earlier backup is never replaced.
fn free_backup_path(hook: &Path) -> Option<PathBuf> {
    let first = backup_path(hook);
    if std::fs::symlink_metadata(&first).is_err() {
        return Some(first);
    }
    (2u32..10_000)
        .map(|n| {
            let mut name = first.as_os_str().to_owned();
            name.push(format!(".{n}"));
            PathBuf::from(name)
        })
        .find(|candidate| std::fs::symlink_metadata(candidate).is_err())
}

/// Installs the pre-commit hook for `project`, running `regia` (see [`hook_script`]). A hook Regia
/// wrote is rewritten; another hook is refused unless `force`, which moves it aside
/// ([`HOOK_BACKUP_SUFFIX`]).
///
/// # Errors
///
/// [`PrecommitError::ForeignHook`] without `force`; git or file errors.
pub fn install_hook(project: &Path, regia: &Path, force: bool) -> Result<HookChange, PrecommitError> {
    let state = hook_state(project)?;
    let root = repo_root(project)?;
    let project_rel = relative_to(project, &root).unwrap_or_else(|| ".".to_owned());
    let foreign = state.exists && !state.regia;
    if foreign && !force {
        return Err(PrecommitError::ForeignHook(state.path));
    }
    // The new script is written first: a failed write leaves every file as it was, and the foreign
    // hook moves aside only once the script is ready (review sprint-w12-rev-g5 #17).
    let tmp = write_temp(&state.path, &hook_script(regia, &project_rel))?;
    let mut backup = None;
    if foreign {
        let Some(to) = free_backup_path(&state.path) else {
            let _ = std::fs::remove_file(&tmp);
            return Err(PrecommitError::Io {
                path: backup_path(&state.path),
                message: "no free backup name for the existing hook".to_owned(),
            });
        };
        if let Err(e) = std::fs::rename(&state.path, &to) {
            let _ = std::fs::remove_file(&tmp);
            return Err(io_error(&state.path, &e));
        }
        backup = Some(to);
    }
    if let Err(e) = std::fs::rename(&tmp, &state.path) {
        let _ = std::fs::remove_file(&tmp);
        // Put the foreign hook back where git looks for it.
        if let Some(to) = &backup {
            let _ = std::fs::rename(to, &state.path);
        }
        return Err(io_error(&state.path, &e));
    }
    let action = if !state.exists {
        "installed"
    } else if state.regia {
        "updated"
    } else {
        "replaced"
    };
    Ok(HookChange {
        path: state.path,
        action,
        backup,
    })
}

/// Removes Regia's pre-commit hook of `project` and puts back a hook it replaced. Another hook is
/// refused unless `force`.
///
/// # Errors
///
/// [`PrecommitError::NoHook`] when there is none, [`PrecommitError::ForeignHook`] for another hook
/// without `force`; git or file errors.
pub fn uninstall_hook(project: &Path, force: bool) -> Result<HookChange, PrecommitError> {
    let state = hook_state(project)?;
    if !state.exists {
        return Err(PrecommitError::NoHook(state.path));
    }
    if !state.regia && !force {
        return Err(PrecommitError::ForeignHook(state.path));
    }
    std::fs::remove_file(&state.path).map_err(|e| io_error(&state.path, &e))?;
    let backup = backup_path(&state.path);
    if state.backup {
        std::fs::rename(&backup, &state.path).map_err(|e| io_error(&backup, &e))?;
        return Ok(HookChange {
            path: state.path,
            action: "restored",
            backup: Some(backup),
        });
    }
    Ok(HookChange {
        path: state.path,
        action: "removed",
        backup: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_status_rows_and_asset_paths() {
        let out = b"A\0assets/a.material.ron\0M\0assets/b.png.meta\0D\0assets/c.glb\0T\0x\0";
        let rows = parse_name_status(out);
        assert_eq!(
            rows,
            [
                (StagedStatus::Added, "assets/a.material.ron".to_owned()),
                (StagedStatus::Modified, "assets/b.png.meta".to_owned()),
                (StagedStatus::Deleted, "assets/c.glb".to_owned()),
                (StagedStatus::Modified, "x".to_owned()),
            ]
        );
        assert_eq!(asset_path("assets/b.png.meta"), "assets/b.png");
        assert_eq!(asset_path("config/validation.ron"), "config/validation.ron");
    }

    #[test]
    fn syntax_checks_ron_and_json() {
        assert_eq!(
            parse_error(b"Material(base_color: (1.0, 0.5, 0.0, 1.0))", Syntax::Ron),
            None
        );
        assert_eq!(parse_error(b"(format_version: 1, id: \"x\")", Syntax::Ron), None);
        assert!(parse_error(b"Material(base_color: (1.0, ", Syntax::Ron).is_some());
        assert!(parse_error(&[0xff, 0xfe], Syntax::Ron).is_some());
        assert_eq!(
            parse_error(b"{\"asset\": {\"version\": \"2.0\"}}", Syntax::Json),
            None
        );
        assert!(parse_error(b"{\"asset\": ", Syntax::Json).is_some());
        assert_eq!(syntax_of("a/B.GLTF"), Some(Syntax::Json));
        assert_eq!(syntax_of("a/b.png"), None);
    }

    #[test]
    fn the_script_quotes_its_paths() {
        let script = hook_script(Path::new("/opt/it's here/regia"), "game dir");
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.contains(HOOK_MARKER));
        assert!(script.contains("REGIA='/opt/it'\\''s here/regia'"), "{script}");
        assert!(script.contains("--staged --project 'game dir'"), "{script}");
        let windows = hook_script(Path::new("C:\\tools\\regia.exe"), ".");
        assert!(windows.contains("REGIA='C:/tools/regia.exe'"), "{windows}");
    }
}
