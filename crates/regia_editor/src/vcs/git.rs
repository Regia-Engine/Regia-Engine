// SPDX-License-Identifier: MPL-2.0
//! The git driver of the Revision Control panel (ADR-7837): every operation runs git's own
//! command-line program with an argument vector (never a shell string), in the repository's top
//! directory, with no terminal prompt and no credential handling. Paths in and out are relative to
//! the project root, `/`-separated; the driver maps them to and from the repository when the
//! project is a subdirectory of it.
//!
//! The status parser reads the documented `git status --porcelain=v2 -z` record layout
//! (`1`/`2`/`u`/`?`/`!` records and `# branch.*` headers, from git's public `git-status` manual).

use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use super::{
    Branches, Change, EntryKind, FileDiff, FileStatus, LogEntry, RepoStatus, RevisionControl, VcsError,
};

/// How many history rows one log call answers at most.
pub const LOG_LIMIT_MAX: usize = 500;
/// The largest text diff the driver hands back (longer ones are cut and marked truncated).
pub const DIFF_TEXT_MAX: usize = 512 * 1024;

/// The git program: the `vcs.git_command` preference when set, else `git` found on `PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitProgram(
    /// The program: a path, or a name looked up on `PATH`.
    pub PathBuf,
);

impl Default for GitProgram {
    fn default() -> Self {
        Self(PathBuf::from("git"))
    }
}

impl GitProgram {
    /// The program named by a preference value; empty means `git` on `PATH`.
    #[must_use]
    pub fn from_pref(value: &str) -> Self {
        let value = value.trim();
        if value.is_empty() {
            Self::default()
        } else {
            Self(PathBuf::from(value))
        }
    }

    /// A command for this program in `cwd` with the environment every call gets: no terminal
    /// prompt, no inherited repository overrides, stdin closed unless the caller pipes it.
    fn command(&self, cwd: &Path) -> Command {
        let mut command = Command::new(&self.0);
        command
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_PREFIX")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// Runs `args` in `cwd`; answers stdout on exit status 0.
    pub(super) fn run(&self, cwd: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>, VcsError> {
        let _span = tracing::debug_span!("vcs_git", op = args.first().copied().unwrap_or("")).entered();
        let mut command = self.command(cwd);
        command.args(args);
        if input.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command.spawn().map_err(|e| spawn_error(&self.0, &e))?;
        if let Some(bytes) = input
            && let Some(mut stdin) = child.stdin.take()
        {
            stdin
                .write_all(bytes)
                .map_err(|e| VcsError::new("git_failed", format!("writing to git: {e}")))?;
        }
        let output = child
            .wait_with_output()
            .map_err(|e| VcsError::new("git_failed", format!("waiting for git: {e}")))?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(VcsError::new("git_failed", failure_text(args, &output.stderr)))
        }
    }

    /// Whether `args` exits with status 0 (a query whose answer is its exit status).
    fn succeeds(&self, cwd: &Path, args: &[&str]) -> Result<bool, VcsError> {
        let output = self
            .command(cwd)
            .args(args)
            .output()
            .map_err(|e| spawn_error(&self.0, &e))?;
        Ok(output.status.success())
    }
}

fn spawn_error(program: &Path, error: &std::io::Error) -> VcsError {
    if error.kind() == std::io::ErrorKind::NotFound {
        VcsError::new(
            "git_not_found",
            format!(
                "git was not found ({}); install git or set the preference vcs.git_command",
                program.display()
            ),
        )
    } else {
        VcsError::new(
            "git_failed",
            format!("could not run {}: {error}", program.display()),
        )
    }
}

fn failure_text(args: &[&str], stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let first: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).take(4).collect();
    let op = args.first().copied().unwrap_or("git");
    if first.is_empty() {
        format!("git {op} failed")
    } else {
        format!("git {op}: {}", first.join(" / "))
    }
}

/// A project path checked and spelled with `/`: relative, inside the project, no `..`, not an
/// option-looking name.
///
/// # Errors
/// `invalid_path` when the path is empty, absolute, leaves the project or starts with `-`.
pub fn clean_project_path(path: &str) -> Result<String, VcsError> {
    let spelled = path.trim().replace('\\', "/");
    let bad = |why: &str| VcsError::new("invalid_path", format!("{path:?}: {why}"));
    if spelled.is_empty() {
        return Err(bad("empty path"));
    }
    if spelled.starts_with('-') {
        return Err(bad("a path may not start with '-'"));
    }
    if spelled.contains('\0') {
        return Err(bad("NUL in path"));
    }
    let as_path = Path::new(&spelled);
    let mut parts = Vec::new();
    for component in as_path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir => return Err(bad("'..' is not allowed")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(bad(
                    "absolute paths are not allowed; use a path relative to the project",
                ));
            }
        }
    }
    if parts.is_empty() {
        return Err(bad("empty path"));
    }
    Ok(parts.join("/"))
}

/// A git repository the project lives in.
#[derive(Debug, Clone)]
pub struct GitDriver {
    program: GitProgram,
    /// The repository's top directory.
    top: PathBuf,
    /// The project directory relative to `top`, `/`-terminated, or empty.
    prefix: String,
    /// The project directory.
    project: PathBuf,
}

impl GitDriver {
    /// Finds the repository `project` is in.
    ///
    /// # Errors
    /// `no_revision_control` when it is in none, `git_not_found` when the program is missing,
    /// `git_failed` otherwise.
    pub fn open(project: &Path, program: GitProgram) -> Result<Self, VcsError> {
        if !project.is_dir() {
            return Err(VcsError::new(
                "no_project",
                format!("{} is not a directory", project.display()),
            ));
        }
        let output = program
            .command(project)
            .args(["rev-parse", "--show-toplevel", "--show-prefix"])
            .output()
            .map_err(|e| spawn_error(&program.0, &e))?;
        if !output.status.success() {
            let text = String::from_utf8_lossy(&output.stderr);
            if text.contains("not a git repository") {
                return Err(VcsError::new(
                    "no_revision_control",
                    "No revision control: the project is not in a git repository",
                ));
            }
            return Err(VcsError::new(
                "git_failed",
                failure_text(&["rev-parse"], &output.stderr),
            ));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut lines = text.lines();
        let top = lines.next().unwrap_or_default().trim().to_string();
        if top.is_empty() {
            // A bare repository or a `.git` directory answers no work tree.
            return Err(VcsError::new(
                "no_revision_control",
                "No revision control: the project is not in a git work tree",
            ));
        }
        let mut prefix = lines.next().unwrap_or_default().trim().replace('\\', "/");
        if !prefix.is_empty() && !prefix.ends_with('/') {
            prefix.push('/');
        }
        Ok(Self {
            program,
            top: PathBuf::from(top),
            prefix,
            project: project.to_path_buf(),
        })
    }

    /// The repository's top directory.
    #[must_use]
    pub fn top(&self) -> &Path {
        &self.top
    }

    /// The project directory relative to the repository, `/`-terminated (empty at the top).
    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    fn git(&self, args: &[&str]) -> Result<Vec<u8>, VcsError> {
        self.program.run(&self.top, args, None)
    }

    fn git_text(&self, args: &[&str]) -> Result<String, VcsError> {
        self.git(args).map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    /// The repository spelling of a checked project path.
    fn repo_path(&self, project_path: &str) -> String {
        format!("{}{project_path}", self.prefix)
    }

    /// The project spelling of a repository path, `None` outside the project.
    fn project_path(&self, repo_path: &str) -> Option<String> {
        if self.prefix.is_empty() {
            return Some(repo_path.to_string());
        }
        repo_path
            .strip_prefix(&self.prefix)
            .filter(|rest| !rest.is_empty())
            .map(str::to_string)
    }

    fn checked(&self, paths: &[String]) -> Result<Vec<String>, VcsError> {
        if paths.is_empty() {
            return Err(VcsError::new("invalid_path", "no paths given"));
        }
        paths
            .iter()
            .map(|p| clean_project_path(p).map(|p| self.repo_path(&p)))
            .collect()
    }

    fn has_head(&self) -> Result<bool, VcsError> {
        self.program
            .succeeds(&self.top, &["rev-parse", "--verify", "--quiet", "HEAD"])
    }

    /// Runs a pathspec command: `args`, then `--`, then the paths (literal pathspecs).
    fn with_paths(&self, args: &[&str], paths: &[String]) -> Result<Vec<u8>, VcsError> {
        let mut all: Vec<&str> = vec!["--literal-pathspecs"];
        all.extend_from_slice(args);
        all.push("--");
        all.extend(paths.iter().map(String::as_str));
        self.git(&all)
    }

    fn check_branch_name(&self, name: &str) -> Result<String, VcsError> {
        let name = name.trim();
        if name.is_empty() || name.starts_with('-') {
            return Err(VcsError::new(
                "invalid_branch",
                format!("{name:?} is not a branch name"),
            ));
        }
        let checked = self
            .program
            .succeeds(&self.top, &["check-ref-format", "--branch", name])?;
        if !checked {
            return Err(VcsError::new(
                "invalid_branch",
                format!("{name:?} is not a valid branch name"),
            ));
        }
        Ok(name.to_string())
    }

    fn sizes_for_binary(
        &self,
        repo: &str,
        staged: bool,
        status: Option<&FileStatus>,
    ) -> (Option<u64>, Option<u64>) {
        let blob_size = |spec: String| -> Option<u64> {
            self.git_text(&["cat-file", "-s", spec.as_str()])
                .ok()
                .and_then(|t| t.trim().parse().ok())
        };
        if staged {
            let old_repo = status
                .and_then(|s| s.orig_path.as_deref())
                .map_or_else(|| repo.to_string(), |orig| self.repo_path(orig));
            (
                blob_size(format!("HEAD:{old_repo}")),
                blob_size(format!(":{repo}")),
            )
        } else {
            let new = std::fs::metadata(self.top.join(repo)).ok().map(|m| m.len());
            (blob_size(format!(":{repo}")), new)
        }
    }
}

impl RevisionControl for GitDriver {
    fn system(&self) -> &'static str {
        "git"
    }

    fn status(&self) -> Result<RepoStatus, VcsError> {
        let _span = tracing::info_span!("vcs_status").entered();
        let mut args = vec![
            "--no-optional-locks",
            "--literal-pathspecs",
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--untracked-files=all",
            "--ignored=matching",
        ];
        if !self.prefix.is_empty() {
            args.push("--");
            args.push(&self.prefix);
        }
        let bytes = self.git(&args)?;
        let mut status = parse_porcelain_v2(&bytes);
        status.files = status
            .files
            .into_iter()
            .filter_map(|mut file| {
                file.path = self.project_path(&file.path)?;
                file.orig_path = file.orig_path.and_then(|p| self.project_path(&p));
                Some(file)
            })
            .collect();
        status.files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(status)
    }

    fn stage(&self, paths: &[String]) -> Result<(), VcsError> {
        let paths = self.checked(paths)?;
        self.with_paths(&["add", "-A"], &paths).map(|_| ())
    }

    fn unstage(&self, paths: &[String]) -> Result<(), VcsError> {
        let paths = self.checked(paths)?;
        if self.has_head()? {
            self.with_paths(&["reset", "-q", "HEAD"], &paths).map(|_| ())
        } else {
            self.with_paths(&["rm", "--cached", "-r", "-q", "--ignore-unmatch"], &paths)
                .map(|_| ())
        }
    }

    fn discard(&self, paths: &[String]) -> Result<Vec<String>, VcsError> {
        let checked: Vec<String> = paths
            .iter()
            .map(|p| clean_project_path(p))
            .collect::<Result<_, _>>()?;
        if checked.is_empty() {
            return Err(VcsError::new("invalid_path", "no paths given"));
        }
        let status = self.status()?;
        let mut tracked = Vec::new();
        let mut untracked = Vec::new();
        let mut done = Vec::new();
        for path in &checked {
            match status.file(path) {
                Some(file) if file.kind == EntryKind::Untracked => untracked.push(path.clone()),
                Some(file) if file.kind == EntryKind::Tracked && file.unstaged != Change::Unmodified => {
                    tracked.push(self.repo_path(path));
                    done.push(path.clone());
                }
                Some(file) if file.kind == EntryKind::Conflicted => {
                    return Err(VcsError::new(
                        "conflicts",
                        format!("{path} has a merge conflict; resolve it with git first"),
                    ));
                }
                _ => {}
            }
        }
        if !tracked.is_empty() {
            self.with_paths(&["checkout", "-q"], &tracked)?;
        }
        for path in untracked {
            let full = self.project.join(&path);
            if full.is_file() {
                std::fs::remove_file(&full)
                    .map_err(|e| VcsError::new("git_failed", format!("removing {path}: {e}")))?;
                done.push(path);
            }
        }
        done.sort();
        Ok(done)
    }

    fn commit(&self, message: &str) -> Result<String, VcsError> {
        let message = message.trim();
        if message.is_empty() {
            return Err(VcsError::new("empty_message", "a commit needs a message"));
        }
        let status = self.status()?;
        if status.files.iter().any(|f| f.kind == EntryKind::Conflicted) {
            return Err(VcsError::new(
                "conflicts",
                "the working tree has merge conflicts; resolve them with git first",
            ));
        }
        if status.staged().next().is_none() {
            return Err(VcsError::new(
                "nothing_staged",
                "nothing is staged; stage changes first",
            ));
        }
        // The message goes through stdin (`-F -`): no command-line length limit, no quoting.
        let mut text = message.to_string();
        text.push('\n');
        self.program.run(
            &self.top,
            &["commit", "-q", "--cleanup=strip", "-F", "-"],
            Some(text.as_bytes()),
        )?;
        Ok(self.git_text(&["rev-parse", "HEAD"])?.trim().to_string())
    }

    fn log(&self, path: Option<&str>, limit: usize) -> Result<Vec<LogEntry>, VcsError> {
        if !self.has_head()? {
            return Ok(Vec::new());
        }
        let limit = format!("--max-count={}", limit.clamp(1, LOG_LIMIT_MAX));
        let mut args: Vec<String> = vec![
            "--literal-pathspecs".into(),
            "log".into(),
            "--no-color".into(),
            "-z".into(),
            "--format=%H%x1f%an%x1f%ae%x1f%aI%x1f%s".into(),
            limit,
        ];
        match path {
            Some(path) => {
                let path = clean_project_path(path)?;
                args.push("--follow".into());
                args.push("--".into());
                args.push(self.repo_path(&path));
            }
            None if !self.prefix.is_empty() => {
                args.push("--".into());
                args.push(self.prefix.clone());
            }
            None => {}
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        Ok(parse_log(&self.git(&refs)?))
    }

    fn branches(&self) -> Result<Branches, VcsError> {
        let names = self.git_text(&["for-each-ref", "--format=%(refname:short)", "refs/heads/"])?;
        let mut list: Vec<String> = names
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        let current = self
            .program
            .run(&self.top, &["symbolic-ref", "--quiet", "--short", "HEAD"], None)
            .ok()
            .map(|b| String::from_utf8_lossy(&b).trim().to_string())
            .filter(|s| !s.is_empty());
        // An unborn branch (no commit yet) has no ref but is the current branch.
        if let Some(current) = &current
            && !list.contains(current)
        {
            list.push(current.clone());
        }
        list.sort();
        Ok(Branches { current, list })
    }

    fn switch_branch(&self, name: &str) -> Result<(), VcsError> {
        let name = self.check_branch_name(name)?;
        let branches = self.branches()?;
        if !branches.list.contains(&name) {
            return Err(VcsError::new(
                "unknown_branch",
                format!("there is no branch {name:?}"),
            ));
        }
        if branches.current.as_deref() == Some(name.as_str()) {
            return Ok(());
        }
        let status = self.status()?;
        if status.is_dirty() {
            return Err(VcsError::new(
                "dirty_tree",
                "the working tree has uncommitted changes to tracked files; commit or discard them before switching",
            ));
        }
        self.git(&["checkout", "-q", name.as_str(), "--"]).map(|_| ())
    }

    fn create_branch(&self, name: &str, switch: bool) -> Result<(), VcsError> {
        let name = self.check_branch_name(name)?;
        if self.branches()?.list.contains(&name) {
            return Err(VcsError::new(
                "branch_exists",
                format!("branch {name:?} already exists"),
            ));
        }
        if switch {
            self.git(&["checkout", "-q", "-b", name.as_str()]).map(|_| ())
        } else {
            self.git(&["branch", name.as_str()]).map(|_| ())
        }
    }

    fn diff(&self, path: &str, staged: bool) -> Result<FileDiff, VcsError> {
        let path = clean_project_path(path)?;
        let repo = self.repo_path(&path);
        let status = self.status()?;
        let entry = status.file(&path).cloned();
        let mut diff = FileDiff {
            path: path.clone(),
            staged,
            kind: "unchanged",
            text: String::new(),
            old_size: None,
            new_size: None,
            truncated: false,
        };
        // An untracked file: all of it is new.
        if !staged && entry.as_ref().is_some_and(|e| e.kind == EntryKind::Untracked) {
            let bytes = std::fs::read(self.project.join(&path))
                .map_err(|e| VcsError::new("git_failed", format!("reading {path}: {e}")))?;
            diff.new_size = Some(bytes.len() as u64);
            match std::str::from_utf8(&bytes) {
                Ok(text) if !bytes.contains(&0) => {
                    diff.kind = "text";
                    let mut out = format!("new file {path}\n");
                    for line in text.lines() {
                        out.push('+');
                        out.push_str(line);
                        out.push('\n');
                    }
                    diff.set_text(out);
                }
                _ => diff.kind = "binary",
            }
            return Ok(diff);
        }
        let mut paths = vec![repo.clone()];
        if staged && let Some(orig) = entry.as_ref().and_then(|e| e.orig_path.as_deref()) {
            paths.push(self.repo_path(orig));
        }
        let cached: &[&str] = if staged { &["--cached"] } else { &[] };
        let mut numstat_args = vec!["diff", "--no-color", "--no-ext-diff", "-M", "--numstat"];
        numstat_args.extend_from_slice(cached);
        let numstat = String::from_utf8_lossy(&self.with_paths(&numstat_args, &paths)?).into_owned();
        if numstat.trim().is_empty() {
            return Ok(diff);
        }
        if numstat.lines().any(|l| l.starts_with("-\t-\t")) {
            diff.kind = "binary";
            let (old, new) = self.sizes_for_binary(&repo, staged, entry.as_ref());
            diff.old_size = old;
            diff.new_size = new;
            return Ok(diff);
        }
        let mut text_args = vec!["diff", "--no-color", "--no-ext-diff", "-M"];
        text_args.extend_from_slice(cached);
        let text = String::from_utf8_lossy(&self.with_paths(&text_args, &paths)?).into_owned();
        diff.kind = "text";
        diff.set_text(text);
        Ok(diff)
    }
}

/// Parses `git status --porcelain=v2 -z --branch` output (paths as git spells them, repository
/// relative).
#[must_use]
pub fn parse_porcelain_v2(bytes: &[u8]) -> RepoStatus {
    let mut status = RepoStatus::default();
    let mut fields = bytes
        .split(|b| *b == 0)
        .map(|f| String::from_utf8_lossy(f).into_owned());
    while let Some(record) = fields.next() {
        if record.is_empty() {
            continue;
        }
        if let Some(header) = record.strip_prefix("# ") {
            if let Some(head) = header.strip_prefix("branch.head ") {
                status.branch = (head != "(detached)").then(|| head.to_string());
                status.detached = head == "(detached)";
            } else if let Some(oid) = header.strip_prefix("branch.oid ") {
                status.head = (oid != "(initial)").then(|| oid.to_string());
            }
            continue;
        }
        let kind = record.as_bytes()[0];
        match kind {
            b'1' | b'2' | b'u' => {
                // Fields before the path: 1 → 8, 2 → 9, u → 10.
                let skip = match kind {
                    b'1' => 8,
                    b'2' => 9,
                    _ => 10,
                };
                let mut parts = record.splitn(skip + 1, ' ');
                let _kind = parts.next();
                let xy = parts.next().unwrap_or("..");
                let path = parts.nth(skip - 2).unwrap_or_default().to_string();
                let mut xy_chars = xy.chars();
                let x = xy_chars.next().unwrap_or('.');
                let y = xy_chars.next().unwrap_or('.');
                let orig_path = if kind == b'2' { fields.next() } else { None };
                if path.is_empty() {
                    continue;
                }
                status.files.push(FileStatus {
                    path,
                    orig_path,
                    staged: if kind == b'u' {
                        Change::Unmodified
                    } else {
                        Change::from_code(x)
                    },
                    unstaged: if kind == b'u' {
                        Change::Unmodified
                    } else {
                        Change::from_code(y)
                    },
                    kind: if kind == b'u' {
                        EntryKind::Conflicted
                    } else {
                        EntryKind::Tracked
                    },
                });
            }
            b'?' | b'!' => {
                let path = record.get(2..).unwrap_or_default().to_string();
                if path.is_empty() {
                    continue;
                }
                status.files.push(FileStatus {
                    path,
                    orig_path: None,
                    staged: Change::Unmodified,
                    unstaged: Change::Unmodified,
                    kind: if kind == b'?' {
                        EntryKind::Untracked
                    } else {
                        EntryKind::Ignored
                    },
                });
            }
            _ => {}
        }
    }
    status
}

/// Parses `git log -z --format=%H%x1f%an%x1f%ae%x1f%aI%x1f%s`.
#[must_use]
pub fn parse_log(bytes: &[u8]) -> Vec<LogEntry> {
    bytes
        .split(|b| *b == 0)
        .filter_map(|record| {
            let text = String::from_utf8_lossy(record);
            let text = text.trim_matches('\n');
            if text.is_empty() {
                return None;
            }
            let mut parts = text.split('\u{1f}');
            Some(LogEntry {
                id: parts.next()?.to_string(),
                author: parts.next().unwrap_or_default().to_string(),
                email: parts.next().unwrap_or_default().to_string(),
                date: parts.next().unwrap_or_default().to_string(),
                summary: parts.next().unwrap_or_default().to_string(),
            })
        })
        .collect()
}
